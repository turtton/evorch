//! Physical purge of the conversation index of archived threads.
//!
//! Archiving a thread evicts its runs from the index with the reason `archived`
//! (see `SqliteThreadIndex::archive_runs`) but keeps their blocks, so the archive
//! itself stays one cheap statement. This tick then
//!
//! 1. evicts, in bounded steps, the runs of threads that were archived before the
//!    mirror hook existed (a persisted cursor makes it resumable), and
//! 2. removes the blocks of `evicted(archived)` runs in bounded transactions.
//!
//! It runs on the writer thread and does not depend on the index job being
//! enabled. A block larger than one step may take is never deleted here: it is
//! registered for the exclusive startup phase (`reclaim_oversized`), whose
//! transactions are sized for one large row, and the run is left alone until then.

use rusqlite::{Connection, params};

use crate::StorageError;
use crate::repo::thread_index::{SqliteThreadIndex, purge_index_rows};
use crate::thread_index::PurgeTarget;
use crate::thread_retention::{RECLAIM_INDEX_ROWS, RECLAIM_INDEX_TEXT_BYTES};

/// Runs purged by one tick; each gets its own transaction.
pub(crate) const ARCHIVE_PURGE_RUNS_PER_TICK: usize = 4;
/// Archived threads whose runs one tick evicts for the backfill.
pub(crate) const ARCHIVE_BACKFILL_THREADS_PER_TICK: usize = 32;
/// The backfill stops after the thread that reaches this many evicted runs.
pub(crate) const ARCHIVE_BACKFILL_RUNS_PER_TICK: usize = 1_000;
/// Family of the `reclaim_oversized` registrations made here. It is no tombstone
/// family, so no reclaim job waits for it.
pub(crate) const ARCHIVE_OVERSIZED_FAMILY: &str = "archive-index";
/// Table name of those registrations (the exclusive phase's index-block entry).
pub(crate) const OVERSIZED_TABLE: &str = "conversation_blocks";

/// What one tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ArchivePurgeProgress {
    /// Runs the purge looked at (including those handed to the exclusive phase).
    pub runs: usize,
    pub rows: usize,
    pub text_bytes: u64,
    /// Runs the backfill evicted.
    pub evicted: usize,
    /// Runs handed to the exclusive phase because a block exceeds the step limit.
    pub registered: usize,
    /// Whether the backfill still has threads to visit.
    pub backfill_pending: bool,
}

/// One bounded step: the backfill, then the purge of the oldest `evicted(archived)`
/// runs that still hold blocks. With nothing to do it only reads.
pub(crate) fn tick(conn: &Connection) -> Result<ArchivePurgeProgress, StorageError> {
    let mut progress = ArchivePurgeProgress::default();
    backfill_step(conn, &mut progress)?;
    let runs: Vec<String> = conn
        .prepare_cached(
            "SELECT run_id FROM thread_index_runs \
             INDEXED BY idx_thread_index_runs_archived_pending \
             WHERE state = 'evicted' AND evicted_reason = 'archived' AND charged_bytes > 0 \
             AND NOT EXISTS (SELECT 1 FROM reclaim_oversized o WHERE o.family_key = ?1 \
                 AND o.tbl = ?2 AND o.key = thread_index_runs.run_id) \
             ORDER BY run_id LIMIT ?3",
        )?
        .query_map(
            params![
                ARCHIVE_OVERSIZED_FAMILY,
                OVERSIZED_TABLE,
                ARCHIVE_PURGE_RUNS_PER_TICK
            ],
            |row| row.get(0),
        )?
        .collect::<Result<_, _>>()?;
    for run_id in runs {
        let step = purge_index_rows(
            conn,
            &run_id,
            PurgeTarget::Archived,
            RECLAIM_INDEX_ROWS,
            RECLAIM_INDEX_TEXT_BYTES,
        )?;
        if step.oversized {
            // Text, notes and tool errors have no extraction limit, so a stored
            // block can be any size. Deleting it here would be an unbounded
            // transaction: hand it to the exclusive phase and skip the run.
            conn.execute(
                "INSERT OR REPLACE INTO reclaim_oversized(family_key, tbl, key, stored_bytes) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    ARCHIVE_OVERSIZED_FAMILY,
                    OVERSIZED_TABLE,
                    run_id,
                    step.oversized_bytes
                ],
            )?;
            progress.registered += 1;
        }
        progress.runs += 1;
        progress.rows += step.rows;
        progress.text_bytes = progress.text_bytes.saturating_add(step.text_bytes);
    }
    Ok(progress)
}

/// Evicts the runs of threads archived before the mirror hook existed. Threads are
/// visited in ID order from a persisted cursor, a few per tick and a bounded number
/// of runs, until none is left; the flag then keeps later ticks from looking again.
/// Search exclusion and the scheduler guards do not wait for it: they read the
/// thread's own flag.
fn backfill_step(
    conn: &Connection,
    progress: &mut ArchivePurgeProgress,
) -> Result<(), StorageError> {
    let (done, cursor): (bool, Option<String>) = conn.query_row(
        "SELECT archive_backfill_done, archive_backfill_cursor FROM thread_index_state \
         WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if done {
        return Ok(());
    }
    let transaction = conn.unchecked_transaction()?;
    let threads: Vec<String> = transaction
        .prepare_cached(
            "SELECT thread_id FROM threads WHERE archived = 1 \
             AND (?1 IS NULL OR thread_id > ?1) ORDER BY thread_id LIMIT ?2",
        )?
        .query_map(params![cursor, ARCHIVE_BACKFILL_THREADS_PER_TICK], |row| {
            row.get(0)
        })?
        .collect::<Result<_, _>>()?;
    let index = SqliteThreadIndex::new(&transaction);
    let mut visited = 0;
    for thread in &threads {
        progress.evicted += index.archive_runs(thread)?;
        visited += 1;
        if progress.evicted >= ARCHIVE_BACKFILL_RUNS_PER_TICK {
            break;
        }
    }
    let finished = visited == threads.len() && threads.len() < ARCHIVE_BACKFILL_THREADS_PER_TICK;
    if finished {
        transaction.execute(
            "UPDATE thread_index_state SET archive_backfill_done = 1, \
             archive_backfill_cursor = NULL WHERE id = 1",
            [],
        )?;
    } else if let Some(last) = visited.checked_sub(1).and_then(|n| threads.get(n)) {
        transaction.execute(
            "UPDATE thread_index_state SET archive_backfill_cursor = ?1 WHERE id = 1",
            [last],
        )?;
    }
    transaction.commit()?;
    progress.backfill_pending = !finished;
    Ok(())
}
