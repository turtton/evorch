//! Fills the `subkind` and `run_id` event columns that migration 30 added, and
//! attributes the Orchestrator events (task progress, task checkpoint, thread goal)
//! that were stored without a `run_id` before `Event::primary_run_id` named one.
//! Retention needs both: a run's rows are found by `run_id`.
//!
//! Opening a database never runs this: converting a long history rewrites
//! every row (including its payload), which can take minutes and write about
//! the size of the database to the WAL. Readers therefore accept unconverted
//! rows (`subkind IS NULL`) and fall back to the payload for them. An explicit
//! maintenance run calls [`Database::backfill_event_columns`], which can be
//! interrupted and repeated: each batch commits on its own and the next call
//! continues with the rows still unconverted.
//!
//! WAL growth: each batch is followed by a passive checkpoint, which copies the
//! frames no reader still needs and lets SQLite reuse the log from its start.
//! With no long-lived reader the log stays within a few batches (about 85 MB
//! for 5,000-row batches on a 2.9 GB history). A reader that keeps an old
//! snapshot open holds the log's tail, so then the WAL keeps growing until it
//! ends: there is no hard size limit. Offline maintenance that needs one must
//! run with the readers and writers stopped.

use event_bus::EventKind;
use rusqlite::{Connection, TransactionBehavior, params};

use crate::{Database, StorageError};

/// Rows converted per transaction by default.
pub const DEFAULT_BACKFILL_BATCH_ROWS: usize = 5_000;

pub(crate) const BATCH_SELECT_SQL: &str = "SELECT id, payload FROM events \
     WHERE subkind IS NULL AND id > ?1 ORDER BY id LIMIT ?2";

/// Orchestrator events stored without a run: the second backfill target. The
/// rows already have a `subkind`, so the first target never revisits them.
pub(crate) const REATTRIBUTE_SELECT_SQL: &str = "SELECT id, payload FROM events \
     WHERE run_id IS NULL \
     AND subkind IN ('TaskProgressed', 'TaskCheckpoint', 'ThreadGoalUpdated') \
     AND id > ?1 ORDER BY id LIMIT ?2";

const LOG_TARGET: &str = "storage::migrate";
/// A progress line is logged every this many batches, and at the end.
const LOG_EVERY_BATCHES: u64 = 10;

/// How much of the backfill one call performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventBackfillOptions {
    /// Rows per transaction. Each commit is followed by a passive WAL
    /// checkpoint, which keeps the WAL short only while no long-lived reader
    /// pins it; the size is not hard-limited.
    pub batch_rows: usize,
    /// Stop after about this many rows, leaving the rest for a later call.
    pub max_rows: Option<u64>,
}

impl Default for EventBackfillOptions {
    fn default() -> Self {
        Self {
            batch_rows: DEFAULT_BACKFILL_BATCH_ROWS,
            max_rows: None,
        }
    }
}

/// What one backfill call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct EventBackfillSummary {
    /// Rows converted by this call.
    pub converted: u64,
    /// Converted rows whose payload no longer decodes. They get an empty
    /// `subkind` and no `run_id`; raw inspection of a run finds them through
    /// `idx_events_blank_subkind` and their payload instead.
    pub undecodable: u64,
    /// Transactions committed by this call.
    pub batches: u64,
    /// Orchestrator events given a `run_id` by this call (second target).
    pub reattributed: u64,
    /// Rows still unconverted or unattributed afterwards.
    pub remaining: u64,
}

impl Database {
    /// Converts rows written before migration 30 in batches of
    /// `options.batch_rows`, committing and passively checkpointing the WAL
    /// after each. Safe to run next to live writers and to repeat after an
    /// interruption.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails, for example when another
    /// writer holds the database longer than the busy timeout.
    pub fn backfill_event_columns(
        &self,
        options: &EventBackfillOptions,
    ) -> Result<EventBackfillSummary, StorageError> {
        backfill(&self.conn, options)
    }

    /// Number of events whose `subkind` and `run_id` are not yet filled in.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn unbackfilled_event_count(&self) -> Result<u64, StorageError> {
        unbackfilled_count(&self.conn)
    }

    /// Number of Orchestrator events (task progress, task checkpoint, thread goal)
    /// that still have no `run_id`.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn unattributed_orchestrator_count(&self) -> Result<u64, StorageError> {
        unattributed_count(&self.conn)
    }
}

pub(crate) fn unattributed_count(conn: &Connection) -> Result<u64, StorageError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE run_id IS NULL \
         AND subkind IN ('TaskProgressed', 'TaskCheckpoint', 'ThreadGoalUpdated')",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(count).map_err(|_| StorageError::OutOfRange("unattributed event count"))
}

/// Whether any row is left for either target; costs one index probe each.
pub(crate) fn backfill_pending(conn: &Connection) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE subkind IS NULL) \
         OR EXISTS(SELECT 1 FROM events WHERE run_id IS NULL \
             AND subkind IN ('TaskProgressed', 'TaskCheckpoint', 'ThreadGoalUpdated'))",
        [],
        |row| row.get(0),
    )?)
}

/// What one bounded backfill step did and what is left.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillProgress {
    pub converted: u64,
    pub reattributed: u64,
    pub remaining_unconverted: u64,
    pub remaining_unattributed: u64,
}

/// Convert and attribute up to `rows` events in total: the unconverted rows
/// first, then the unattributed Orchestrator rows, each target in its own
/// transaction.
pub(crate) fn backfill_step(
    conn: &Connection,
    rows: usize,
) -> Result<BackfillProgress, StorageError> {
    let rows = rows.max(1);
    let mut progress = BackfillProgress::default();
    if let Some(batch) = convert_batch(conn, i64::MIN, rows)? {
        progress.converted = batch.converted;
    }
    let left = rows.saturating_sub(usize::try_from(progress.converted).unwrap_or(usize::MAX));
    if left > 0
        && let Some(batch) = reattribute_batch(conn, i64::MIN, left)?
    {
        progress.reattributed = batch.converted;
    }
    progress.remaining_unconverted = unbackfilled_count(conn)?;
    progress.remaining_unattributed = unattributed_count(conn)?;
    Ok(progress)
}

impl Database {
    /// Run [`backfill_step`]-sized steps of `rows` until about `max_rows` events
    /// were handled or nothing is left. For the exclusive startup phase.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn retention_backfill_startup(
        &self,
        rows: usize,
        max_rows: u64,
    ) -> Result<BackfillProgress, StorageError> {
        let mut total = BackfillProgress::default();
        let mut handled = 0_u64;
        while handled < max_rows {
            let step = backfill_step(&self.conn, rows)?;
            let done = step.converted + step.reattributed;
            total.converted += step.converted;
            total.reattributed += step.reattributed;
            total.remaining_unconverted = step.remaining_unconverted;
            total.remaining_unattributed = step.remaining_unattributed;
            handled += done;
            if done == 0 {
                break;
            }
        }
        if handled == 0 {
            total.remaining_unconverted = unbackfilled_count(&self.conn)?;
            total.remaining_unattributed = unattributed_count(&self.conn)?;
        }
        Ok(total)
    }
}

pub(crate) fn unbackfilled_count(conn: &Connection) -> Result<u64, StorageError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE subkind IS NULL",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(count).map_err(|_| StorageError::OutOfRange("unconverted event count"))
}

fn backfill(
    conn: &Connection,
    options: &EventBackfillOptions,
) -> Result<EventBackfillSummary, StorageError> {
    let batch_rows = options.batch_rows.max(1);
    let mut summary = EventBackfillSummary::default();
    let initial = unbackfilled_count(conn)?;
    if initial > 0 {
        tracing::info!(
            target: LOG_TARGET,
            remaining = initial,
            batch_rows,
            "event column backfill started"
        );
    }
    let mut after_id = i64::MIN;
    loop {
        let budget = match options.max_rows {
            Some(max) => {
                let left = max.saturating_sub(summary.converted);
                if left == 0 {
                    break;
                }
                batch_rows.min(usize::try_from(left).unwrap_or(usize::MAX))
            }
            None => batch_rows,
        };
        let Some(batch) = convert_batch(conn, after_id, budget)? else {
            break;
        };
        after_id = batch.last_id;
        summary.converted += batch.converted;
        summary.undecodable += batch.undecodable;
        summary.batches += 1;
        // Truncating would stall behind readers. A passive checkpoint only moves
        // what no reader needs, so a long-lived reader leaves the WAL growing.
        checkpoint_passive(conn);
        if summary.batches % LOG_EVERY_BATCHES == 0 {
            tracing::info!(
                target: LOG_TARGET,
                converted = summary.converted,
                total = initial,
                "event column backfill progress"
            );
        }
    }
    // Second target: Orchestrator events that predate their run attribution.
    let mut after_id = i64::MIN;
    loop {
        let budget = match options.max_rows {
            Some(max) => {
                let left = max.saturating_sub(summary.converted + summary.reattributed);
                if left == 0 {
                    break;
                }
                batch_rows.min(usize::try_from(left).unwrap_or(usize::MAX))
            }
            None => batch_rows,
        };
        let Some(batch) = reattribute_batch(conn, after_id, budget)? else {
            break;
        };
        after_id = batch.last_id;
        summary.reattributed += batch.converted;
        summary.undecodable += batch.undecodable;
        summary.batches += 1;
        checkpoint_passive(conn);
    }
    summary.remaining = unbackfilled_count(conn)? + unattributed_count(conn)?;
    if summary.batches > 0 {
        tracing::info!(
            target: LOG_TARGET,
            converted = summary.converted,
            undecodable = summary.undecodable,
            remaining = summary.remaining,
            "event column backfill finished"
        );
    }
    Ok(summary)
}

struct Batch {
    last_id: i64,
    converted: u64,
    undecodable: u64,
}

/// Converts up to `limit` rows after `after_id`, or returns `None` when no
/// unconverted row is left.
fn convert_batch(
    conn: &Connection,
    after_id: i64,
    limit: usize,
) -> Result<Option<Batch>, StorageError> {
    let transaction = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let derived = {
        let mut select = transaction.prepare_cached(BATCH_SELECT_SQL)?;
        let mut rows = select.query(params![
            after_id,
            i64::try_from(limit).map_err(|_| StorageError::OutOfRange("backfill batch size"))?
        ])?;
        let mut derived = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            // A payload that cannot even be read back is blank like one that no longer decodes.
            let columns = match crate::codec::decode(row.get_ref(1)?) {
                Ok(payload) => derive_columns(&payload),
                Err(_) => derive_columns(""),
            };
            derived.push((id, columns));
        }
        derived
    };
    let Some(&(last_id, _)) = derived.last() else {
        return Ok(None);
    };
    let mut undecodable = 0_u64;
    {
        let mut update = transaction.prepare_cached(
            "UPDATE events SET subkind = ?2, run_id = ?3 WHERE id = ?1 AND subkind IS NULL",
        )?;
        for (id, columns) in &derived {
            undecodable += u64::from(!columns.decoded);
            update.execute(params![id, columns.subkind, columns.run_id])?;
        }
    }
    transaction.commit()?;
    Ok(Some(Batch {
        last_id,
        converted: u64::try_from(derived.len())
            .map_err(|_| StorageError::OutOfRange("backfill batch size"))?,
        undecodable,
    }))
}

/// Gives up to `limit` unattributed Orchestrator events the run `primary_run_id`
/// names, or returns `None` when none is left. Payloads are read, never rewritten.
/// A payload that no longer decodes becomes blank (`subkind = ''`) like the first
/// target's undecodable rows, so it stops counting as pending.
fn reattribute_batch(
    conn: &Connection,
    after_id: i64,
    limit: usize,
) -> Result<Option<Batch>, StorageError> {
    let transaction = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let derived = {
        let mut select = transaction.prepare_cached(REATTRIBUTE_SELECT_SQL)?;
        let mut rows = select.query(params![
            after_id,
            i64::try_from(limit).map_err(|_| StorageError::OutOfRange("backfill batch size"))?
        ])?;
        let mut derived = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let columns = match crate::codec::decode(row.get_ref(1)?) {
                Ok(payload) => derive_columns(&payload),
                Err(_) => derive_columns(""),
            };
            derived.push((id, columns));
        }
        derived
    };
    let Some(&(last_id, _)) = derived.last() else {
        return Ok(None);
    };
    let mut undecodable = 0_u64;
    {
        let mut update = transaction.prepare_cached(
            "UPDATE events SET subkind = ?2, run_id = ?3 WHERE id = ?1 AND run_id IS NULL",
        )?;
        for (id, columns) in &derived {
            undecodable += u64::from(!columns.decoded);
            update.execute(params![id, columns.subkind, columns.run_id])?;
        }
    }
    transaction.commit()?;
    Ok(Some(Batch {
        last_id,
        converted: u64::try_from(derived.len())
            .map_err(|_| StorageError::OutOfRange("backfill batch size"))?,
        undecodable,
    }))
}

struct Columns {
    subkind: &'static str,
    run_id: Option<String>,
    decoded: bool,
}

/// The stored columns of an event payload.
fn derive_columns(payload: &str) -> Columns {
    match serde_json::from_str::<EventKind>(payload) {
        Ok(kind) => Columns {
            subkind: kind.subkind_name(),
            run_id: kind.primary_run_id().map(String::from),
            decoded: true,
        },
        Err(_) => Columns {
            subkind: "",
            run_id: None,
            decoded: false,
        },
    }
}

fn checkpoint_passive(conn: &Connection) {
    // The result row reports busy/log/checkpointed frames; a partial checkpoint
    // only means the rest is moved by a later one.
    if let Err(error) = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(())) {
        tracing::warn!(target: LOG_TARGET, %error, "passive WAL checkpoint failed");
    }
}
