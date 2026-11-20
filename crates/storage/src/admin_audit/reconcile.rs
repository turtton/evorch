//! Ends unfinished operations from what the restart transactions durably say. It keeps no
//! state machine and no clock of its own: the caller says when, the transactions say what.

use rusqlite::{Transaction, params};

use super::store::{Limits, inconsistent, tag};
use super::{AuditCleanup, AuditOutcome, AuditReconcile, AuditReply, validate};
use crate::StorageError;

/// A restart transaction's phase and Ready record, as far as the verdict needs them.
fn verdict(tx: &Transaction<'_>, restart_id: Option<&str>) -> Result<AuditOutcome, StorageError> {
    let Some(restart_id) = restart_id else {
        return Ok(AuditOutcome::Unknown);
    };
    let found: Option<(String, Option<i64>)> =
        rusqlite::OptionalExtension::optional(tx.query_row(
            "SELECT phase, ready_at_ms FROM restart_transactions WHERE restart_id = ?1",
            [restart_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ))?;
    Ok(match found {
        // Ready was recorded: the successor served.
        Some((_, Some(_))) => AuditOutcome::Succeeded,
        Some((phase, None))
            if matches!(phase.as_str(), "aborted" | "invalidated" | "superseded") =>
        {
            AuditOutcome::Failed
        }
        // Completed without a Ready record, an unfinished restart or no transaction at
        // all: success is never inferred.
        _ => AuditOutcome::Unknown,
    })
}

pub(crate) fn step(
    tx: &Transaction<'_>,
    mode: &AuditReconcile,
    now_ms: u64,
    limits: &Limits,
) -> Result<AuditReply, StorageError> {
    validate::db_u64(now_ms, "now_ms is beyond the storable range")?;
    // One more than the batch tells whether anything is left. Every selected row is
    // ended, so the next step always sees fewer rows.
    let fetch = i64::try_from(limits.batch + 1).unwrap_or(i64::MAX);
    let selected: Vec<(String, Option<String>)> = match mode {
        AuditReconcile::Cold { live_core_epoch } => {
            validate::db_u64(
                *live_core_epoch,
                "live_core_epoch is beyond the storable range",
            )?;
            let mut statement = tx.prepare(
                "SELECT operation_id, restart_id FROM accepted_admin_operations \
                 WHERE state != 'finished' AND accepted_core_epoch != ?1 \
                 ORDER BY accepted_at_ms, operation_id LIMIT ?2",
            )?;
            statement
                .query_map(params![live_core_epoch, fetch], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<_, _>>()?
        }
        AuditReconcile::Ready { restart_id } => {
            validate::restart_id(restart_id)?;
            let mut statement = tx.prepare(
                "SELECT a.operation_id, a.restart_id FROM accepted_admin_operations a \
                 JOIN restart_transactions t ON t.restart_id = a.restart_id \
                 WHERE a.state != 'finished' AND a.operation = 'restart_request' \
                 AND a.restart_id = ?1 AND t.ready_at_ms IS NOT NULL \
                 ORDER BY a.accepted_at_ms, a.operation_id LIMIT ?2",
            )?;
            statement
                .query_map(params![restart_id, fetch], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<_, _>>()?
        }
    };
    let more = selected.len() > limits.batch;
    let mut reconciled = 0;
    for (operation_id, restart_id) in selected.into_iter().take(limits.batch) {
        let outcome = verdict(tx, restart_id.as_deref())?;
        let changed = tx.execute(
            "UPDATE accepted_admin_operations SET state = 'finished', outcome = ?2, \
             finished_at_ms = ?3 WHERE operation_id = ?1 AND state != 'finished'",
            params![operation_id, tag(&outcome)?, now_ms],
        )?;
        if changed != 1 {
            return Err(inconsistent());
        }
        reconciled += 1;
    }
    Ok(AuditReply::Cleanup(AuditCleanup {
        accepted_removed: 0,
        denials_removed: 0,
        reconciled,
        more,
    }))
}
