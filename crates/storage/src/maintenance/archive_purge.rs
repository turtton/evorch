//! Physical purge of the conversation index of archived threads.
//!
//! Archiving a thread evicts its runs from the index with the reason `archived`
//! (see `SqliteThreadIndex::archive_runs`) but keeps their blocks, so the archive
//! itself stays one cheap statement. This tick then
//!
//! 1. evicts, in bounded steps, the runs of threads that were archived before the
//!    mirror hook existed (a persisted position inside the thread makes it
//!    resumable, whatever the thread's size), and
//! 2. removes the blocks of `evicted(archived)` runs in bounded transactions.
//!
//! It runs on the writer thread and does not depend on the index job being
//! enabled. A block larger than one step may take is never deleted here: it is
//! registered for the exclusive startup phase (`reclaim_oversized`), whose
//! transactions are sized for one large row, and the run is left alone until then.

use rusqlite::{Connection, OptionalExtension, params};

use crate::StorageError;
use crate::repo::thread_index::{SqliteThreadIndex, purge_index_rows};
use crate::thread_index::PurgeTarget;
use crate::thread_retention::{RECLAIM_INDEX_ROWS, RECLAIM_INDEX_TEXT_BYTES};

/// Runs purged by one tick; each gets its own transaction.
pub(crate) const ARCHIVE_PURGE_RUNS_PER_TICK: usize = 4;
/// Index rows (and visited threads) one backfill tick reads and updates, at most.
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
             AND purge_deferred = 0 ORDER BY run_id LIMIT ?1",
        )?
        .query_map([ARCHIVE_PURGE_RUNS_PER_TICK], |row| row.get(0))?
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
            let transaction = conn.unchecked_transaction()?;
            transaction.execute(
                "INSERT OR REPLACE INTO reclaim_oversized(family_key, tbl, key, stored_bytes) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    ARCHIVE_OVERSIZED_FAMILY,
                    OVERSIZED_TABLE,
                    run_id,
                    step.oversized_bytes
                ],
            )?;
            // The flag takes the run out of the partial index the tick reads, so
            // later ticks never look at it again (and its count never matters).
            transaction.execute(
                "UPDATE thread_index_runs SET purge_deferred = 1 WHERE run_id = ?1",
                [&run_id],
            )?;
            transaction.commit()?;
            progress.registered += 1;
        }
        progress.runs += 1;
        progress.rows += step.rows;
        progress.text_bytes = progress.text_bytes.saturating_add(step.text_bytes);
    }
    Ok(progress)
}

/// One step of the backfill. Its position is persisted as the next archived thread
/// and, inside it, the last run done. A tick reads and updates at most
/// [`ARCHIVE_BACKFILL_RUNS_PER_TICK`] index rows, however many runs one thread has:
/// each statement is a keyset range over an index (threads by ID, one thread's runs
/// by ID), never a filtered scan, so its `LIMIT` is also its scan bound.
/// Search exclusion and the scheduler guards do not wait for it: they read the
/// thread's own flag.
fn backfill_step(
    conn: &Connection,
    progress: &mut ArchivePurgeProgress,
) -> Result<(), StorageError> {
    let (done, mut thread, mut after): (bool, Option<String>, String) = conn.query_row(
        "SELECT archive_backfill_done, archive_backfill_thread, archive_backfill_run \
         FROM thread_index_state WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if done {
        return Ok(());
    }
    let transaction = conn.unchecked_transaction()?;
    let index = SqliteThreadIndex::new(&transaction);
    let next_thread = |after_thread: &str| -> Result<Option<String>, StorageError> {
        Ok(transaction
            .prepare_cached(
                "SELECT thread_id FROM threads INDEXED BY idx_threads_archived_by_id \
                 WHERE archived = 1 AND thread_id > ?1 ORDER BY thread_id LIMIT 1",
            )?
            .query_row([after_thread], |row| row.get(0))
            .optional()?)
    };
    let mut budget = ARCHIVE_BACKFILL_RUNS_PER_TICK;
    let mut finished = false;
    while budget > 0 {
        let current = match thread.take() {
            Some(current) => current,
            None => match next_thread("")? {
                Some(first) => first,
                None => {
                    finished = true;
                    break;
                }
            },
        };
        let still_archived: bool = transaction
            .prepare_cached("SELECT archived FROM threads WHERE thread_id = ?1")?
            .query_row([&current], |row| row.get(0))
            .optional()?
            .unwrap_or(false);
        let mut exhausted = true;
        if still_archived {
            if after.is_empty() {
                progress.evicted += index.archive_thread_root(&current)?;
            }
            let limit = budget;
            let chunk: Vec<String> = transaction
                .prepare_cached(
                    "SELECT run_id FROM thread_runs INDEXED BY idx_thread_runs_thread_run \
                     WHERE thread_id = ?1 AND run_id > ?2 ORDER BY run_id LIMIT ?3",
                )?
                .query_map(params![current, after, limit], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            if let Some(last) = chunk.last() {
                progress.evicted += index.archive_thread_runs_between(&current, &after, last)?;
                exhausted = chunk.len() < limit;
                after = last.clone();
            }
            // A full chunk may be the end of the thread; the next tick finds out.
            budget = budget.saturating_sub(chunk.len());
        }
        // Every thread visited costs one unit, so a run of empty threads is bounded too.
        budget = budget.saturating_sub(1);
        if exhausted {
            after = String::new();
            match next_thread(&current)? {
                Some(next) => thread = Some(next),
                None => {
                    finished = true;
                    break;
                }
            }
        } else {
            thread = Some(current);
        }
    }
    if finished {
        transaction.execute(
            "UPDATE thread_index_state SET archive_backfill_done = 1, \
             archive_backfill_thread = NULL, archive_backfill_run = '' WHERE id = 1",
            [],
        )?;
    } else {
        transaction.execute(
            "UPDATE thread_index_state SET archive_backfill_thread = ?1, \
             archive_backfill_run = ?2 WHERE id = 1",
            params![thread, after],
        )?;
    }
    transaction.commit()?;
    progress.backfill_pending = !finished;
    Ok(())
}
