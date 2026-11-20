//! The journal of administrative operations: every command runs in one IMMEDIATE
//! transaction on the writer's connection and is acknowledged after its commit.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{
    AUDIT_BATCH_ROWS, AUDIT_COMPLETED_RETENTION_MS, AUDIT_MAX_ACCEPTED_BYTES,
    AUDIT_MAX_ACCEPTED_ROWS, AUDIT_MAX_RECORD_BYTES, AuditAccepted, AuditCapacity, AuditCleanup,
    AuditConflict, AuditError, AuditOperationKey, AuditOutcome, AuditRecord, AuditReply,
    AuditReservation, AuditState, AuditStats, AuditWrite, DENIAL_MAX_BYTES,
    DENIAL_MAX_KEYS_PER_WINDOW, DENIAL_MAX_ROWS, DENIAL_RETENTION_MS, denials, reconcile, validate,
};
use crate::StorageError;

/// What a finished row gains later (restart ID, outcome, time) is reserved up front, so
/// the budget of a row never changes and no transition can exceed the record limit.
pub(super) const RESERVE_BYTES: u64 = 512;

/// The bounds of the store. Production always uses [`Limits::default`]; the unit tests
/// shrink them, because the real byte bounds cannot be reached with valid field sizes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub accepted_rows: u64,
    pub accepted_bytes: u64,
    pub record_bytes: u64,
    pub completed_retention_ms: u64,
    pub denial_rows: u64,
    pub denial_bytes: u64,
    pub denial_retention_ms: u64,
    pub denial_keys_per_window: usize,
    pub batch: usize,
    /// Rows one write may evict to make room; the same bound as a batch in production.
    pub evictions: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            accepted_rows: AUDIT_MAX_ACCEPTED_ROWS,
            accepted_bytes: AUDIT_MAX_ACCEPTED_BYTES,
            record_bytes: AUDIT_MAX_RECORD_BYTES,
            completed_retention_ms: AUDIT_COMPLETED_RETENTION_MS,
            denial_rows: DENIAL_MAX_ROWS,
            denial_bytes: DENIAL_MAX_BYTES,
            denial_retention_ms: DENIAL_RETENTION_MS,
            denial_keys_per_window: DENIAL_MAX_KEYS_PER_WINDOW,
            batch: AUDIT_BATCH_ROWS,
            evictions: AUDIT_BATCH_ROWS,
        }
    }
}

pub(super) fn inconsistent() -> StorageError {
    StorageError::Serialization("administrative audit row is inconsistent".into())
}

pub(super) fn serialize<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| StorageError::Serialization(error.to_string()))
}

pub(super) fn tag<T: Serialize>(value: &T) -> Result<String, StorageError> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => Ok(text),
        _ => Err(inconsistent()),
    }
}

pub(super) fn parse_tag<T: DeserializeOwned>(text: &str) -> Result<T, StorageError> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).map_err(|_| inconsistent())
}

/// Whether `at + retention <= now`, without overflow.
pub(super) fn expired(at_ms: u64, retention_ms: u64, now_ms: u64) -> bool {
    now_ms
        .checked_sub(retention_ms)
        .is_some_and(|cutoff| at_ms <= cutoff)
}

/// The largest timestamp that is expired at `now_ms`, or none yet.
pub(super) fn cutoff(retention_ms: u64, now_ms: u64) -> Option<i64> {
    now_ms
        .checked_sub(retention_ms)
        .map(|cutoff| i64::try_from(cutoff).unwrap_or(i64::MAX))
}

pub(crate) fn execute(conn: &Connection, write: AuditWrite) -> Result<AuditReply, StorageError> {
    execute_with(conn, write, &Limits::default())
}

pub(crate) fn execute_with(
    conn: &Connection,
    write: AuditWrite,
    limits: &Limits,
) -> Result<AuditReply, StorageError> {
    match write {
        AuditWrite::Read { operation_id } => {
            validate::operation_id(&operation_id)?;
            Ok(AuditReply::Record(read(conn, &operation_id)?))
        }
        AuditWrite::Stats => Ok(AuditReply::Stats(stats(conn)?)),
        other => {
            let durable = !matches!(
                other,
                AuditWrite::Denials { .. } | AuditWrite::Cleanup { .. }
            );
            let previous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
            let raise = durable && previous != 2;
            if raise {
                conn.pragma_update(None, "synchronous", "FULL")?;
            }
            let result = mutate(conn, other, limits);
            if raise && let Err(error) = conn.pragma_update(None, "synchronous", previous) {
                tracing::warn!(error = %error, "could not restore the synchronous setting");
            }
            result
        }
    }
}

fn mutate(
    conn: &Connection,
    write: AuditWrite,
    limits: &Limits,
) -> Result<AuditReply, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let reply = match write {
        AuditWrite::Accept(accepted) => accept(&tx, accepted, limits)?,
        AuditWrite::Start { key } => start(&tx, &key)?,
        AuditWrite::BindRestart { key, restart_id } => bind_restart(&tx, &key, &restart_id)?,
        AuditWrite::Finalize {
            key,
            outcome,
            at_ms,
        } => finalize(&tx, &key, outcome, at_ms)?,
        AuditWrite::Denials { rows, now_ms } => denials::write(&tx, rows, now_ms, limits)?,
        AuditWrite::Cleanup { now_ms } => cleanup(&tx, now_ms, limits)?,
        AuditWrite::Reconcile { mode, now_ms } => reconcile::step(&tx, &mode, now_ms, limits)?,
        AuditWrite::Read { .. } | AuditWrite::Stats => return Err(inconsistent()),
    };
    #[cfg(feature = "admin-audit-fault")]
    super::fault::before_commit()?;
    tx.commit()?;
    Ok(reply)
}

struct RawRow {
    operation_id: String,
    attempt_nonce: String,
    accepted_core_epoch: u64,
    operation: String,
    accepted_json: String,
    state: String,
    restart_id: Option<String>,
    outcome: Option<String>,
    finished_at_ms: Option<u64>,
}

const COLUMNS: &str = "operation_id, attempt_nonce, accepted_core_epoch, operation, \
    accepted_json, state, restart_id, outcome, finished_at_ms";

fn raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        operation_id: row.get(0)?,
        attempt_nonce: row.get(1)?,
        accepted_core_epoch: row.get(2)?,
        operation: row.get(3)?,
        accepted_json: row.get(4)?,
        state: row.get(5)?,
        restart_id: row.get(6)?,
        outcome: row.get(7)?,
        finished_at_ms: row.get(8)?,
    })
}

fn record(row: RawRow) -> Result<AuditRecord, StorageError> {
    let accepted: AuditAccepted =
        serde_json::from_str(&row.accepted_json).map_err(|_| inconsistent())?;
    validate::accepted(&accepted).map_err(|_| inconsistent())?;
    if accepted.key.operation_id != row.operation_id
        || accepted.key.attempt_nonce != row.attempt_nonce
        || accepted.accepted_core_epoch != row.accepted_core_epoch
        || tag(&accepted.operation)? != row.operation
    {
        return Err(inconsistent());
    }
    let state: AuditState = parse_tag(&row.state)?;
    let outcome: Option<AuditOutcome> = row.outcome.as_deref().map(parse_tag).transpose()?;
    if (state == AuditState::Finished) != outcome.is_some()
        || outcome.is_some() != row.finished_at_ms.is_some()
    {
        return Err(inconsistent());
    }
    if let Some(restart_id) = &row.restart_id {
        validate::restart_id(restart_id).map_err(|_| inconsistent())?;
        if accepted.operation != super::AuditOperation::RestartRequest {
            return Err(inconsistent());
        }
    }
    Ok(AuditRecord {
        accepted,
        state,
        restart_id: row.restart_id,
        outcome,
        finished_at_ms: row.finished_at_ms,
    })
}

pub(crate) fn read(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<AuditRecord>, StorageError> {
    validate::operation_id(operation_id)?;
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM accepted_admin_operations WHERE operation_id = ?1"),
        [operation_id],
        raw,
    )
    .optional()?
    .map(record)
    .transpose()
}

fn totals(conn: &Connection) -> Result<(u64, u64), StorageError> {
    Ok(conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(budget_bytes), 0) FROM accepted_admin_operations",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

pub(crate) fn stats(conn: &Connection) -> Result<AuditStats, StorageError> {
    let (accepted_count, accepted_bytes, pending_count) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(budget_bytes), 0), \
         COALESCE(SUM(state != 'finished'), 0) FROM accepted_admin_operations",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let (denial_count, denial_bytes) = denials::totals(conn)?;
    Ok(AuditStats {
        accepted_count,
        accepted_bytes,
        pending_count,
        denial_count,
        denial_bytes,
    })
}

fn same_facts(left: &AuditAccepted, right: &AuditAccepted) -> bool {
    left.operation == right.operation
        && left.surface == right.surface
        && left.principal == right.principal
        && left.accepted_core_epoch == right.accepted_core_epoch
        && left.intent_digest == right.intent_digest
}

/// Frees room for one row of `budget` bytes by removing the oldest finished rows, at most
/// one batch. Unfinished rows are never candidates.
fn make_room(conn: &Connection, budget: u64, limits: &Limits) -> Result<(), StorageError> {
    let (mut rows, mut bytes) = totals(conn)?;
    let mut evicted = 0;
    while rows + 1 > limits.accepted_rows || bytes + budget > limits.accepted_bytes {
        if evicted >= limits.evictions {
            return Err(AuditError::Capacity(AuditCapacity::PendingOperations).into());
        }
        let oldest: Option<(String, u64)> = conn
            .query_row(
                "SELECT operation_id, budget_bytes FROM accepted_admin_operations \
                 WHERE state = 'finished' ORDER BY finished_at_ms, operation_id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, freed)) = oldest else {
            return Err(AuditError::Capacity(AuditCapacity::PendingOperations).into());
        };
        conn.execute(
            "DELETE FROM accepted_admin_operations WHERE operation_id = ?1",
            [&id],
        )?;
        rows -= 1;
        bytes -= freed;
        evicted += 1;
    }
    Ok(())
}

fn accept(
    tx: &Transaction<'_>,
    accepted: AuditAccepted,
    limits: &Limits,
) -> Result<AuditReply, StorageError> {
    validate::accepted(&accepted)?;
    if let Some(existing) = read(tx, &accepted.key.operation_id)? {
        return if same_facts(&existing.accepted, &accepted) {
            Ok(AuditReply::Reservation(AuditReservation::Existing(
                existing,
            )))
        } else {
            Err(AuditError::Conflict(AuditConflict::SemanticMismatch).into())
        };
    }
    let json = serialize(&accepted)?;
    let budget = json.len() as u64 + RESERVE_BYTES;
    if budget > limits.record_bytes {
        return Err(AuditError::Invalid("the record exceeds the record limit").into());
    }
    make_room(tx, budget, limits)?;
    tx.execute(
        "INSERT INTO accepted_admin_operations(operation_id, attempt_nonce, accepted_core_epoch, \
         operation, accepted_at_ms, accepted_json, state, budget_bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'prepared', ?7)",
        params![
            accepted.key.operation_id,
            accepted.key.attempt_nonce,
            accepted.accepted_core_epoch,
            tag(&accepted.operation)?,
            accepted.accepted_at_ms,
            json,
            budget,
        ],
    )?;
    let created = read(tx, &accepted.key.operation_id)?.ok_or_else(inconsistent)?;
    Ok(AuditReply::Reservation(AuditReservation::Created(created)))
}

/// The row of `key`'s operation, refused unless the attempt is the caller's.
fn owned(tx: &Transaction<'_>, key: &AuditOperationKey) -> Result<AuditRecord, StorageError> {
    validate::key(key)?;
    let row = read(tx, &key.operation_id)?
        .ok_or(AuditError::Conflict(AuditConflict::MissingOperation))?;
    if row.accepted.key.attempt_nonce != key.attempt_nonce {
        return Err(AuditError::Conflict(AuditConflict::NonceMismatch).into());
    }
    Ok(row)
}

fn start(tx: &Transaction<'_>, key: &AuditOperationKey) -> Result<AuditReply, StorageError> {
    let row = owned(tx, key)?;
    match row.state {
        AuditState::Started => Ok(AuditReply::Applied(row)),
        AuditState::Finished => Err(AuditError::Conflict(AuditConflict::InvalidTransition).into()),
        AuditState::Prepared => {
            tx.execute(
                "UPDATE accepted_admin_operations SET state = 'started' \
                 WHERE operation_id = ?1 AND state = 'prepared'",
                [&key.operation_id],
            )?;
            applied(tx, key)
        }
    }
}

fn applied(tx: &Transaction<'_>, key: &AuditOperationKey) -> Result<AuditReply, StorageError> {
    Ok(AuditReply::Applied(
        read(tx, &key.operation_id)?.ok_or_else(inconsistent)?,
    ))
}

fn bind_restart(
    tx: &Transaction<'_>,
    key: &AuditOperationKey,
    restart_id: &str,
) -> Result<AuditReply, StorageError> {
    validate::restart_id(restart_id)?;
    let row = owned(tx, key)?;
    if row.accepted.operation != super::AuditOperation::RestartRequest {
        return Err(AuditError::Conflict(AuditConflict::NotRestartOperation).into());
    }
    match row.restart_id.as_deref() {
        Some(bound) if bound == restart_id => Ok(AuditReply::Applied(row)),
        Some(_) => Err(AuditError::Conflict(AuditConflict::RestartIdMismatch).into()),
        None if row.state == AuditState::Finished => {
            Err(AuditError::Conflict(AuditConflict::InvalidTransition).into())
        }
        None => {
            tx.execute(
                "UPDATE accepted_admin_operations SET restart_id = ?2 \
                 WHERE operation_id = ?1 AND restart_id IS NULL AND state != 'finished'",
                params![key.operation_id, restart_id],
            )?;
            applied(tx, key)
        }
    }
}

fn finalize(
    tx: &Transaction<'_>,
    key: &AuditOperationKey,
    outcome: AuditOutcome,
    at_ms: u64,
) -> Result<AuditReply, StorageError> {
    validate::db_u64(at_ms, "at_ms is beyond the storable range")?;
    let row = owned(tx, key)?;
    if row.state == AuditState::Finished {
        return if row.outcome == Some(outcome) {
            Ok(AuditReply::Applied(row))
        } else {
            Err(AuditError::Conflict(AuditConflict::OutcomeMismatch).into())
        };
    }
    tx.execute(
        "UPDATE accepted_admin_operations SET state = 'finished', outcome = ?2, finished_at_ms = ?3 \
         WHERE operation_id = ?1 AND state != 'finished'",
        params![key.operation_id, tag(&outcome)?, at_ms],
    )?;
    applied(tx, key)
}

fn cleanup(tx: &Transaction<'_>, now_ms: u64, limits: &Limits) -> Result<AuditReply, StorageError> {
    validate::db_u64(now_ms, "now_ms is beyond the storable range")?;
    let batch = i64::try_from(limits.batch).unwrap_or(i64::MAX);
    let mut accepted_removed = 0;
    let mut more = false;
    if let Some(cutoff) = cutoff(limits.completed_retention_ms, now_ms) {
        accepted_removed = tx.execute(
            "DELETE FROM accepted_admin_operations WHERE operation_id IN (\
             SELECT operation_id FROM accepted_admin_operations \
             WHERE state = 'finished' AND finished_at_ms <= ?1 \
             ORDER BY finished_at_ms, operation_id LIMIT ?2)",
            params![cutoff, batch],
        )? as u64;
        more = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM accepted_admin_operations \
             WHERE state = 'finished' AND finished_at_ms <= ?1)",
            [cutoff],
            |row| row.get::<_, bool>(0),
        )?;
    }
    let (denials_removed, denials_more) = denials::cleanup(tx, now_ms, limits)?;
    Ok(AuditReply::Cleanup(AuditCleanup {
        accepted_removed,
        denials_removed,
        reconciled: 0,
        more: more || denials_more,
    }))
}
