use std::time::{Duration, SystemTime};

use super::{WriterState, file_sizes, refresh_size_state};
use crate::repo::event::{self, diagnostic};
use crate::{
    DIAGNOSTIC_RETENTION_DAYS, DiagnosticCleanupScope, DiagnosticCleanupSummary, StorageError,
};

pub(super) fn status(
    state: &WriterState,
    scope: DiagnosticCleanupScope,
) -> Result<crate::writer::cleanup::CleanupStatus, StorageError> {
    Ok(crate::writer::cleanup::CleanupStatus {
        through: diagnostic::last_cursor(&state.conn, scope)?,
        total_bytes: file_sizes(&state.config.db_path)?.total(),
        freelist_pages: super::freelist_pages(&state.conn)?,
        requires_full_vacuum: auto_vacuum_mode(&state.conn)? == 0,
        writes_suspended: state.writes_suspended,
    })
}

pub(super) fn cleanup_batch(
    state: &mut WriterState,
    scope: DiagnosticCleanupScope,
    after: Option<diagnostic::Cursor>,
    through: diagnostic::Cursor,
) -> Result<(DiagnosticCleanupSummary, Option<diagnostic::Cursor>), StorageError> {
    let result = diagnostic::cleanup_batch(&state.conn, scope, after, through)?;
    // Our own commit does not increment data_version. Invalidate quota caches
    // before replying and allowing the next ordinary writer command to run.
    state.accounting = event::EventAccounting::default();
    state.diagnostic_cursor = None;
    Ok(result)
}

pub(super) fn reclaim_step(
    state: &mut WriterState,
    requested_pages: u64,
) -> Result<crate::writer::cleanup::ReclaimStep, StorageError> {
    // This command does at most 256 pages of vacuum before returning to normal
    // commands. The caller requeues continuation after receiving this reply.
    const PAGE_BUDGET: u64 = 256;
    let pages = requested_pages
        .min(PAGE_BUDGET)
        .min(super::freelist_pages(&state.conn)?);
    if pages > 0 && auto_vacuum_mode(&state.conn)? == 2 {
        vacuum_pages(&state.conn, pages as i64)?;
    }
    let maintenance_error = if reclaim_wal(state)? {
        None
    } else {
        Some("WAL reclamation is deferred while another connection is reading or writing".into())
    };
    let total_bytes = file_sizes(&state.config.db_path)?.total();
    refresh_size_state(state, total_bytes);
    Ok(crate::writer::cleanup::ReclaimStep {
        pages_attempted: pages,
        total_bytes,
        writes_suspended: state.writes_suspended,
        maintenance_error,
    })
}

pub(super) fn retain_recent(state: &mut WriterState, now: SystemTime) -> Result<u64, StorageError> {
    let cutoff = crate::system_time_to_ns(now)?
        .saturating_sub((DIAGNOSTIC_RETENTION_DAYS * 86_400 * 1_000_000_000) as i64);
    let summary = diagnostic::retention_batch(&state.conn, cutoff, &mut state.diagnostic_cursor)?;
    if summary.event_count > 0 {
        state.accounting = event::EventAccounting::default();
        tracing::info!(
            events_removed = summary.event_count,
            payload_bytes_removed = summary.payload_bytes,
            "expired diagnostic history removed"
        );
    }
    Ok(summary.event_count)
}

fn auto_vacuum_mode(conn: &rusqlite::Connection) -> Result<i64, StorageError> {
    Ok(conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?)
}

/// incremental_vacuum yields rows while making progress; drain them to execute
/// the requested page budget instead of stopping after its first step.
pub(super) fn vacuum_pages(conn: &rusqlite::Connection, pages: i64) -> Result<(), StorageError> {
    debug_assert!(pages > 0);
    let mut statement = conn.prepare(&format!("PRAGMA incremental_vacuum({pages})"))?;
    let mut rows = statement.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

/// A live reader may pin the WAL. Never block the writer waiting for it to end;
/// restore the usual busy handler and let a later maintenance tick retry.
pub(super) fn reclaim_wal(state: &mut WriterState) -> Result<bool, StorageError> {
    state.reclaim_wal_pending = true;
    let reclaimed = truncate_wal_without_wait(&state.conn)?;
    state.reclaim_wal_pending = !reclaimed;
    Ok(reclaimed)
}

fn truncate_wal_without_wait(conn: &rusqlite::Connection) -> Result<bool, StorageError> {
    let timeout_ms: u32 = conn.pragma_query_value(None, "busy_timeout", |row| row.get(0))?;
    conn.busy_timeout(Duration::ZERO)?;
    let result = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    });
    conn.busy_timeout(Duration::from_millis(u64::from(timeout_ms)))?;
    Ok(result? == 0)
}

#[cfg(test)]
#[path = "diagnostic_tests.rs"]
mod tests;
