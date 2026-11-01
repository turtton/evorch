//! Physical purge of the conversation index of archived threads.
//!
//! Archiving a thread evicts its runs from the index with the reason
//! `archived` (see `SqliteThreadIndex::archive_runs`) but keeps their blocks, so
//! the archive itself stays one cheap statement. This step then removes the
//! blocks in bounded transactions, a few runs per maintenance tick. It runs on
//! the writer thread and does not depend on the index job being enabled.

use rusqlite::Connection;

use crate::StorageError;
use crate::repo::thread_index::{OversizedBlock, purge_index_rows, purge_index_rows_with};
use crate::thread_index::PurgeTarget;
use crate::thread_retention::{RECLAIM_INDEX_ROWS, RECLAIM_INDEX_TEXT_BYTES};

/// Runs purged by one tick; each gets its own transaction.
pub(crate) const ARCHIVE_PURGE_RUNS_PER_TICK: usize = 4;

/// What one tick removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ArchivePurgeProgress {
    pub runs: usize,
    pub rows: usize,
    pub text_bytes: u64,
}

/// One bounded purge step over the oldest `evicted(archived)` runs that still
/// hold blocks. With nothing to purge it only reads.
pub(crate) fn tick(conn: &Connection) -> Result<ArchivePurgeProgress, StorageError> {
    let runs: Vec<String> = conn
        .prepare_cached(
            "SELECT run_id FROM thread_index_runs \
             INDEXED BY idx_thread_index_runs_archived_pending WHERE state = 'evicted' AND evicted_reason = 'archived' AND charged_bytes > 0 \
             ORDER BY run_id LIMIT ?1",
        )?
        .query_map([ARCHIVE_PURGE_RUNS_PER_TICK], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut progress = ArchivePurgeProgress::default();
    for run_id in runs {
        let mut step = purge_index_rows(
            conn,
            &run_id,
            PurgeTarget::Archived,
            RECLAIM_INDEX_ROWS,
            RECLAIM_INDEX_TEXT_BYTES,
        )?;
        if step.oversized {
            // Extraction truncates every block, so one block is the unit of work.
            step = purge_index_rows_with(
                conn,
                &run_id,
                PurgeTarget::Archived,
                RECLAIM_INDEX_ROWS,
                RECLAIM_INDEX_TEXT_BYTES,
                OversizedBlock::TakeAlone,
            )?;
        }
        progress.runs += 1;
        progress.rows += step.rows;
        progress.text_bytes = progress.text_bytes.saturating_add(step.text_bytes);
    }
    Ok(progress)
}
