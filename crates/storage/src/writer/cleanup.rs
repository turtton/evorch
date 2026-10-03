//! Caller-side orchestration keeps long manual work off the writer command loop.

use rusqlite::{Connection, OpenFlags};

use super::{Command, StorageHandle};
use crate::repo::event::diagnostic::{self, Cursor};
use crate::{DiagnosticCleanupScope, DiagnosticCleanupSummary, StorageError};

pub(super) struct CleanupStatus {
    pub through: Option<Cursor>,
    pub total_bytes: u64,
    pub freelist_pages: u64,
    pub requires_full_vacuum: bool,
    pub writes_suspended: bool,
}

pub(super) struct ReclaimStep {
    pub pages_attempted: u64,
    pub total_bytes: u64,
    pub writes_suspended: bool,
    pub maintenance_error: Option<String>,
}

impl StorageHandle {
    /// Count disposable diagnostics on a separate read-only connection. The
    /// consistent preview snapshot does not occupy the single writer thread.
    /// Audit records and unrecognized payloads are excluded.
    pub fn preview_diagnostic_cleanup(
        &self,
        scope: DiagnosticCleanupScope,
    ) -> Result<DiagnosticCleanupSummary, StorageError> {
        let conn = Connection::open_with_flags(&self.2.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut summary = diagnostic::preview(&conn, scope)?;
        summary.requires_full_vacuum =
            conn.pragma_query_value(None, "auto_vacuum", |row| row.get::<_, i64>(0))? == 0;
        summary.writes_suspended =
            crate::db::file_sizes(&self.2.db_path)?.total() >= self.2.hard_limits.max_db_bytes;
        Ok(summary)
    }

    /// Remove selected diagnostics in bounded committed writer batches, yielding
    /// to queued writes between batches and between vacuum page budgets.
    /// Concurrent arrivals can make preview counts differ from deleted counts.
    /// An error before any deletion returns Err. If later work fails, Ok reports
    /// exactly the committed deletion count and describes unfinished work in
    /// maintenance_error. Each failed batch is rolled back independently.
    pub fn cleanup_diagnostics(
        &self,
        scope: DiagnosticCleanupScope,
    ) -> Result<DiagnosticCleanupSummary, StorageError> {
        let before = self.request(|reply| Command::DiagnosticCleanupStatus(scope, reply))?;
        let mut summary = DiagnosticCleanupSummary {
            requires_full_vacuum: before.requires_full_vacuum,
            writes_suspended: before.writes_suspended,
            ..Default::default()
        };
        if let Some(through) = before.through {
            let mut cursor = None;
            loop {
                match self
                    .request(|reply| Command::CleanupDiagnosticBatch(scope, cursor, through, reply))
                {
                    Ok((batch, next)) => {
                        summary.event_count += batch.event_count;
                        summary.payload_bytes += batch.payload_bytes;
                        cursor = next;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    Err(error) if summary.event_count == 0 => return Err(error),
                    Err(error) => {
                        summary.maintenance_error = Some(format!(
                            "Cleanup stopped after {} committed deletions; some diagnostics remain: {error}",
                            summary.event_count
                        ));
                        break;
                    }
                }
            }
        }

        let after = match self.request(|reply| Command::DiagnosticCleanupStatus(scope, reply)) {
            Ok(status) => status,
            Err(error) => return incomplete(summary, error),
        };
        // Capture a finite reclamation budget. Later application writes cannot
        // make this manual request chase an indefinitely growing freelist.
        let mut pages = if after.requires_full_vacuum {
            0
        } else {
            after.freelist_pages
        };
        loop {
            match self.request(|reply| Command::ReclaimDiagnosticSpace(pages, reply)) {
                Ok(step) => {
                    summary.reclaimed_bytes = before.total_bytes.saturating_sub(step.total_bytes);
                    summary.writes_suspended = step.writes_suspended;
                    if let Some(error) = step.maintenance_error {
                        add_issue(&mut summary, error);
                        break;
                    }
                    pages = pages.saturating_sub(step.pages_attempted);
                    if pages == 0 || step.pages_attempted == 0 {
                        break;
                    }
                }
                Err(error) => return incomplete(summary, error),
            }
        }
        Ok(summary)
    }
}

fn add_issue(summary: &mut DiagnosticCleanupSummary, error: String) {
    match &mut summary.maintenance_error {
        Some(previous) => {
            previous.push_str("; ");
            previous.push_str(&error);
        }
        None => summary.maintenance_error = Some(error),
    }
}

fn incomplete(
    mut summary: DiagnosticCleanupSummary,
    error: StorageError,
) -> Result<DiagnosticCleanupSummary, StorageError> {
    if summary.event_count == 0 {
        return Err(error);
    }
    add_issue(&mut summary, error.to_string());
    Ok(summary)
}
