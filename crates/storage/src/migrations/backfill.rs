//! Fills the `subkind` and `run_id` event columns that migration 30 added.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventBackfillSummary {
    /// Rows converted by this call.
    pub converted: u64,
    /// Converted rows whose payload no longer decodes. They get an empty
    /// `subkind`, and as `run_id` the first `run_id` string found in the raw
    /// payload (keys in sorted order, depth first), so that raw inspection
    /// still finds them by run.
    pub undecodable: u64,
    /// Transactions committed by this call.
    pub batches: u64,
    /// Rows still unconverted afterwards.
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
    summary.remaining = unbackfilled_count(conn)?;
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
            let payload: String = row.get(1)?;
            derived.push((id, derive_columns(&payload)));
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

struct Columns {
    subkind: &'static str,
    run_id: Option<String>,
    decoded: bool,
}

/// The stored columns of an event payload. A payload the current types reject
/// still names its run in raw JSON, which inspection of old events relies on.
fn derive_columns(payload: &str) -> Columns {
    match serde_json::from_str::<EventKind>(payload) {
        Ok(kind) => Columns {
            subkind: kind.subkind_name(),
            run_id: kind.primary_run_id().map(String::from),
            decoded: true,
        },
        Err(_) => Columns {
            subkind: "",
            run_id: serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|value| first_run_id(&value)),
            decoded: false,
        },
    }
}

fn first_run_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(object) => {
            object
                .iter()
                .find_map(|(key, value)| match (key.as_str(), value.as_str()) {
                    ("run_id", Some(run_id)) => Some(run_id.to_owned()),
                    _ => first_run_id(value),
                })
        }
        serde_json::Value::Array(items) => items.iter().find_map(first_run_id),
        _ => None,
    }
}

fn checkpoint_passive(conn: &Connection) {
    // The result row reports busy/log/checkpointed frames; a partial checkpoint
    // only means the rest is moved by a later one.
    if let Err(error) = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(())) {
        tracing::warn!(target: LOG_TARGET, %error, "passive WAL checkpoint failed");
    }
}
