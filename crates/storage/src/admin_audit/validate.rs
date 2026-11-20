//! Shape checks for everything that enters or leaves the audit tables.

use super::{AuditAccepted, AuditError, AuditOperationKey, AuditPrincipal, DenialAggregate};
use super::{DENIAL_WINDOW_MS, DenialKey};

/// Largest value a SQLite integer column holds.
pub(super) const SQLITE_MAX: u64 = i64::MAX as u64;

fn one_line(value: &str, max: usize, what: &'static str) -> Result<(), AuditError> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(AuditError::Invalid(what));
    }
    Ok(())
}

pub(super) fn operation_id(value: &str) -> Result<(), AuditError> {
    one_line(
        value,
        128,
        "operation_id must be 1 to 128 bytes on one line",
    )
}

pub(super) fn restart_id(value: &str) -> Result<(), AuditError> {
    one_line(value, 128, "restart_id must be 1 to 128 bytes on one line")
}

/// A lowercase canonical UUID v4.
pub(super) fn nonce(value: &str) -> Result<(), AuditError> {
    const WHAT: &str = "attempt_nonce must be a lowercase UUID v4";
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return Err(AuditError::Invalid(WHAT));
    }
    for (index, byte) in bytes.iter().enumerate() {
        let ok = match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            14 => *byte == b'4',
            19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
        };
        if !ok {
            return Err(AuditError::Invalid(WHAT));
        }
    }
    Ok(())
}

pub(super) fn digest(value: &str) -> Result<(), AuditError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(AuditError::Invalid(
            "intent_digest must be 64 lowercase hex characters",
        ));
    }
    Ok(())
}

pub(super) fn principal(value: &AuditPrincipal) -> Result<(), AuditError> {
    one_line(
        &value.method,
        32,
        "principal.method must be 1 to 32 bytes on one line",
    )?;
    one_line(
        &value.subject,
        256,
        "principal.subject must be 1 to 256 bytes on one line",
    )?;
    one_line(
        &value.binding,
        128,
        "principal.binding must be 1 to 128 bytes on one line",
    )
}

pub(super) fn key(value: &AuditOperationKey) -> Result<(), AuditError> {
    operation_id(&value.operation_id)?;
    nonce(&value.attempt_nonce)
}

pub(super) fn db_u64(value: u64, what: &'static str) -> Result<(), AuditError> {
    if value > SQLITE_MAX {
        return Err(AuditError::Invalid(what));
    }
    Ok(())
}

pub(super) fn accepted(value: &AuditAccepted) -> Result<(), AuditError> {
    key(&value.key)?;
    principal(&value.principal)?;
    digest(&value.intent_digest)?;
    db_u64(
        value.accepted_core_epoch,
        "accepted_core_epoch is beyond the storable range",
    )?;
    db_u64(
        value.accepted_at_ms,
        "accepted_at_ms is beyond the storable range",
    )
}

pub(super) fn denial_key(value: &DenialKey) -> Result<(), AuditError> {
    match value {
        DenialKey::Caller {
            principal: Some(p), ..
        } => principal(p),
        DenialKey::Caller {
            principal: None, ..
        }
        | DenialKey::Overflow => Ok(()),
    }
}

pub(super) fn denial(value: &DenialAggregate) -> Result<(), AuditError> {
    denial_key(&value.key)?;
    if !value.window_start_ms.is_multiple_of(DENIAL_WINDOW_MS) {
        return Err(AuditError::Invalid(
            "window_start_ms is not a window boundary",
        ));
    }
    // Leave room for the window length inside the SQLite integer range.
    db_u64(
        value.window_start_ms.saturating_add(DENIAL_WINDOW_MS),
        "window_start_ms is beyond the storable range",
    )?;
    if value.first_ms < value.window_start_ms
        || value.last_ms < value.first_ms
        || value.last_ms >= value.window_start_ms + DENIAL_WINDOW_MS
    {
        return Err(AuditError::Invalid(
            "first_ms and last_ms must be ordered and inside the window",
        ));
    }
    if value.count == 0 {
        return Err(AuditError::Invalid("count must be at least 1"));
    }
    Ok(())
}
