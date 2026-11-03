//! The retention step of the conversion: rows of runs that a confirmed deletion already
//! removed are reclaimed before anything is converted, with the existing bounded
//! reclaim and the exclusive-phase reclaim. Nothing new is chosen for deletion, and no
//! file outside the database is touched (a family's files stay `files_pending`).

use rusqlite::Connection;
use serde::Serialize;

use crate::StorageError;
use crate::migrations::backfill::backfill_step;
use crate::repo::thread_retention::reclaim::{
    Layout, invalid_cursor_families, reclaim_oversized_step_in, reclaim_step_in,
};
use crate::thread_retention::ReclaimBudget;

use super::super::invalid;
use super::source::table_exists;

/// Transactions of one bounded reclaim call.
const STEP_TRANSACTIONS: u32 = 64;
/// Stored bytes one exclusive-phase call may delete.
const OVERSIZED_STEP_BYTES: u64 = 64 * 1024 * 1024;
/// Events one backfill step handles.
const BACKFILL_ROWS: usize = 5_000;

/// What the retention step did, reported apart from the conversion.
#[derive(Debug, Default, Serialize, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    /// `run_contexts` rows of deleted runs that were reclaimed.
    pub reclaimed_runs: u64,
    /// Rows the reclaim deleted, of every table, and their stored bytes.
    pub reclaimed_rows: u64,
    pub reclaimed_bytes: u64,
    /// Events among the deleted rows.
    pub deleted_events: u64,
    /// Events whose attribution columns were filled so that the reclaim could find them.
    pub backfilled_events: u64,
    /// Jobs whose database rows are gone and whose files a later startup removes.
    pub jobs_files_pending: u64,
}

fn count(conn: &Connection, sql: &str) -> Result<u64, StorageError> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

fn deleted_runs(conn: &Connection) -> Result<u64, StorageError> {
    count(
        conn,
        "SELECT COUNT(*) FROM run_contexts WHERE run_id IN (SELECT run_id FROM run_tombstones)",
    )
}

/// Where the reclaim stands, so a call that changes nothing is noticed.
fn fingerprint(conn: &Connection) -> Result<String, StorageError> {
    Ok(conn.query_row(
        "SELECT COALESCE((SELECT group_concat(family_key || ':' || state || ':' || cursor_json \
                || ':' || reclaimed_rows, '|') FROM reclaim_jobs \
                WHERE state = 'db_pending'), '') \
             || '#' || (SELECT COUNT(*) FROM reclaim_oversized) \
             || '#' || (SELECT COALESCE(SUM(stored_bytes), 0) FROM reclaim_oversized)",
        [],
        |row| row.get(0),
    )?)
}

/// Why the reclaim of deleted runs cannot be completed, found by reading only: a pending
/// job whose cursor this build does not read, and a deleted run that still has its row
/// with no pending job to remove it. Nothing is repaired; the conversion does not start.
pub(super) fn blockers(conn: &Connection) -> Result<Vec<String>, StorageError> {
    // A database older than version 35 has no deletions to reclaim.
    if !table_exists(conn, "reclaim_jobs")? || !table_exists(conn, "run_tombstones")? {
        return Ok(Vec::new());
    }
    let mut found: Vec<String> = invalid_cursor_families(conn)?
        .into_iter()
        .map(|family| format!("family {family}: its reclaim cursor is not one this build reads"))
        .collect();
    let mut statement = conn.prepare(
        "SELECT DISTINCT t.family_key FROM run_tombstones t \
         JOIN run_contexts c ON c.run_id = t.run_id \
         WHERE NOT EXISTS (SELECT 1 FROM reclaim_jobs j \
             WHERE j.family_key = t.family_key AND j.state = 'db_pending') \
         ORDER BY t.family_key",
    )?;
    let families = statement.query_map([], |row| row.get::<_, String>(0))?;
    for family in families {
        found.push(format!(
            "family {}: deleted runs still have rows and no reclaim job is pending",
            family?
        ));
    }
    Ok(found)
}

fn pending(conn: &Connection) -> Result<(u64, u64), StorageError> {
    Ok((
        count(
            conn,
            "SELECT COUNT(*) FROM reclaim_jobs WHERE state = 'db_pending'",
        )?,
        count(conn, "SELECT COUNT(*) FROM reclaim_oversized")?,
    ))
}

fn backfill(conn: &Connection, report: &mut RetentionReport) -> Result<(), StorageError> {
    loop {
        let step = backfill_step(conn, BACKFILL_ROWS)?;
        report.backfilled_events += step.converted + step.reattributed;
        // Each batch is committed; a passive checkpoint keeps the log short.
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        if step.converted + step.reattributed == 0 {
            return Ok(());
        }
    }
}

/// Reclaims every deleted family's database rows, or fails: a conversion never starts on
/// a database that still holds rows of deleted runs. `sample` is called after each step
/// (the caller records the log size).
pub(super) fn reclaim_deleted(
    conn: &Connection,
    sample: &mut dyn FnMut(),
) -> Result<RetentionReport, StorageError> {
    let mut report = RetentionReport::default();
    if let Some(blocker) = blockers(conn)?.first() {
        return Err(invalid(format!(
            "retention reclaim cannot finish: {blocker}"
        )));
    }
    let runs_before = deleted_runs(conn)?;
    let (jobs, oversized) = pending(conn)?;
    if runs_before == 0 && jobs == 0 && oversized == 0 {
        report.jobs_files_pending = files_pending(conn)?;
        return Ok(report);
    }
    let events_before = count(conn, "SELECT COUNT(*) FROM events")?;
    backfill(conn, &mut report)?;
    sample();
    let budget = ReclaimBudget {
        max_transactions: STEP_TRANSACTIONS,
    };
    loop {
        let (jobs, oversized) = pending(conn)?;
        if jobs == 0 && oversized == 0 {
            break;
        }
        let before = fingerprint(conn)?;
        if jobs > 0 {
            let step = reclaim_step_in(conn, budget, Layout::Converting)?;
            if let Some(error) = step.last_error {
                return Err(invalid(format!("retention reclaim failed: {error}")));
            }
            report.reclaimed_rows += step.rows;
            report.reclaimed_bytes += step.stored_bytes;
        }
        let progressed_bounded = fingerprint(conn)? != before;
        let mut progressed_oversized = false;
        let (_, oversized) = pending(conn)?;
        if oversized > 0 {
            let step = reclaim_oversized_step_in(conn, OVERSIZED_STEP_BYTES, Layout::Converting)?;
            if let Some(error) = step.last_error {
                return Err(invalid(format!("retention reclaim failed: {error}")));
            }
            report.reclaimed_rows += step.rows;
            report.reclaimed_bytes += step.stored_bytes;
            progressed_oversized = step.rows > 0 || step.remaining != oversized;
        }
        sample();
        if !progressed_bounded && !progressed_oversized {
            return Err(invalid(
                "retention reclaim made no progress; a deleted family cannot be reclaimed",
            ));
        }
    }
    let left = deleted_runs(conn)?;
    if left != 0 {
        return Err(invalid(format!(
            "{left} run_contexts rows of deleted runs remain after the reclaim"
        )));
    }
    report.reclaimed_runs = runs_before;
    report.deleted_events =
        events_before.saturating_sub(count(conn, "SELECT COUNT(*) FROM events")?);
    report.jobs_files_pending = files_pending(conn)?;
    Ok(report)
}

fn files_pending(conn: &Connection) -> Result<u64, StorageError> {
    count(
        conn,
        "SELECT COUNT(*) FROM reclaim_jobs WHERE state IN ('files_pending', 'files_failed')",
    )
}
