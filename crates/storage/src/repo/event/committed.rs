//! Transcript rows read by id window, so a follower reads only what was appended.

use std::collections::HashSet;

use event_bus::EventKind;
use rusqlite::{Connection, Row, params};

use super::deferred::{DEFERRED_COLUMNS, DeferredRow, deferred_row, failed_row};
use super::kind_name;
use super::select::{Candidate, has_unbackfilled, normalize, push_ids};
use crate::StorageError;
use crate::thread_retention::TombstoneSet;

/// The `(kind, subkind)` pairs of the rows a transcript is built from.
pub const COMMITTED_SUBKINDS: [(&str, &str); 5] = [
    ("Message", "MessageCompleted"),
    ("Message", "FinalResultPublished"),
    ("Message", "MessagePart"),
    ("Lifecycle", "TaskPromptPublished"),
    ("Tool", "ToolCompleted"),
];

/// The columns that tell a stored row from another one written later under the same id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventStamp {
    pub session_id: Option<String>,
    pub monotonic_ns: i64,
    pub wall_clock_ns: i64,
}

const KIND_COLUMN: usize = 5;
const SUBKIND_COLUMN: usize = 8;
const RUN_ID_COLUMN: usize = 9;

pub(crate) const IDS_COMMITTED_BETWEEN_SQL: &str =
    "SELECT id FROM events WHERE kind = ?1 AND subkind = ?2 AND id > ?3 AND id <= ?4";
pub(crate) const IDS_UNCONVERTED_BETWEEN_SQL: &str =
    "SELECT id FROM events WHERE subkind IS NULL AND id > ?1 AND id <= ?2";
// The CASE keeps the planner on the run index: a committed pair alone would let it walk
// every committed row of every run through the kind/subkind index instead.
pub(crate) const IDS_COMMITTED_OF_RUN_SQL: &str = "SELECT id FROM events \
     WHERE run_id = ?1 AND id <= ?2 AND CASE +kind \
     WHEN 'Message' THEN +subkind IN ('MessageCompleted', 'FinalResultPublished', 'MessagePart') \
     WHEN 'Lifecycle' THEN +subkind = 'TaskPromptPublished' \
     WHEN 'Tool' THEN +subkind = 'ToolCompleted' ELSE 0 END";
// A compressed or tokenized payload cannot be searched for the run id, so it stays a
// candidate and decoding confirms it.
pub(crate) const IDS_UNCONVERTED_OF_RUN_SQL: &str = "SELECT id FROM events \
     WHERE subkind IS NULL AND id <= ?2 AND kind IN ('Message', 'Lifecycle', 'Tool') \
     AND (typeof(payload) = 'blob' OR body_format = 1 \
          OR instr(payload, substr(json_quote(?1), 2, length(json_quote(?1)) - 2)) > 0)";
pub(crate) const NEWEST_COMMITTED_SQL: &str = "SELECT id, session_id, monotonic_ns, wall_clock_ns \
     FROM events WHERE kind = ?1 AND subkind = ?2 \
     AND (run_id IS NULL OR run_id NOT IN (SELECT run_id FROM run_tombstones)) \
     ORDER BY id DESC LIMIT 1";
pub(crate) const NEWEST_UNCONVERTED_SQL: &str =
    "SELECT id FROM events WHERE subkind IS NULL AND id > ?1 ORDER BY id DESC";
const STAMP_SQL: &str = "SELECT session_id, monotonic_ns, wall_clock_ns FROM events WHERE id = ?1";

pub(crate) fn is_committed(kind: &EventKind) -> bool {
    COMMITTED_SUBKINDS.contains(&(kind_name(kind), kind.subkind_name()))
}

fn stamp(row: &Row<'_>, [session, monotonic, wall]: [usize; 3]) -> rusqlite::Result<EventStamp> {
    Ok(EventStamp {
        session_id: row.get(session)?,
        monotonic_ns: row.get(monotonic)?,
        wall_clock_ns: row.get(wall)?,
    })
}

const DEFERRED_STAMP: [usize; 3] = [1, 3, 4];

fn fetch_sql() -> String {
    format!("SELECT {DEFERRED_COLUMNS} FROM events WHERE id = ?1")
}

pub(super) fn record_fetched(conn: &Connection, rows: u64) {
    #[cfg(feature = "test-support")]
    crate::test_support::record_event_rows(conn, rows);
    #[cfg(not(feature = "test-support"))]
    let _ = (conn, rows);
}

/// The committed rows of `runs` with `after < id <= through`, in id order, long strings
/// still tokens. An unreadable row is [`DeferredRow::Failed`] only when its `run_id` column
/// names a listed run that is not deleted; an unbackfilled row is handed over only when it
/// decodes to a committed event of a listed run. Tombstoned rows are skipped.
///
/// Returns the newest committed row of the window of any run that is not deleted.
///
/// # Errors
/// Fails only when a query itself fails.
pub fn for_each_committed_event_after(
    conn: &Connection,
    runs: &HashSet<&str>,
    after: i64,
    through: i64,
    visit: impl FnMut(DeferredRow),
) -> Result<Option<(i64, EventStamp)>, StorageError> {
    let (tombstones, candidates) = {
        let transaction = conn.unchecked_transaction()?;
        let tombstones = TombstoneSet::load(&transaction)?;
        let mut candidates = Vec::new();
        for (kind, subkind) in COMMITTED_SUBKINDS {
            push_ids(
                &transaction,
                IDS_COMMITTED_BETWEEN_SQL,
                params![kind, subkind, after, through],
                false,
                &mut candidates,
            )?;
        }
        if has_unbackfilled(&transaction)? {
            push_ids(
                &transaction,
                IDS_UNCONVERTED_BETWEEN_SQL,
                params![after, through],
                true,
                &mut candidates,
            )?;
        }
        (tombstones, normalize(candidates))
    };
    visit_committed(conn, &candidates, runs, &tombstones, visit)
}

/// Every committed row of `run` with `id <= through`, found through the run index, under
/// the rules of [`for_each_committed_event_after`].
///
/// # Errors
/// Fails only when a query itself fails.
pub fn for_each_committed_event_of_run(
    conn: &Connection,
    run: &str,
    through: i64,
    visit: impl FnMut(DeferredRow),
) -> Result<(), StorageError> {
    let (tombstones, candidates) = {
        let transaction = conn.unchecked_transaction()?;
        let tombstones = TombstoneSet::load(&transaction)?;
        let mut candidates = Vec::new();
        push_ids(
            &transaction,
            IDS_COMMITTED_OF_RUN_SQL,
            params![run, through],
            false,
            &mut candidates,
        )?;
        if has_unbackfilled(&transaction)? {
            push_ids(
                &transaction,
                IDS_UNCONVERTED_OF_RUN_SQL,
                params![run, through],
                true,
                &mut candidates,
            )?;
        }
        (tombstones, normalize(candidates))
    };
    visit_committed(conn, &candidates, &HashSet::from([run]), &tombstones, visit)?;
    Ok(())
}

fn visit_committed(
    conn: &Connection,
    candidates: &[Candidate],
    runs: &HashSet<&str>,
    tombstones: &TombstoneSet,
    mut visit: impl FnMut(DeferredRow),
) -> Result<Option<(i64, EventStamp)>, StorageError> {
    let mut statement = conn.prepare_cached(&fetch_sql())?;
    let mut fetched = 0_u64;
    let mut newest = None;
    for candidate in candidates {
        let mut rows = statement.query([candidate.id])?;
        // The row may have been deleted since the ids were collected.
        let Some(row) = rows.next()? else { continue };
        fetched += 1;
        if candidate.confirm {
            let Ok(event) = deferred_row(row) else {
                continue;
            };
            if !is_committed(&event.event.kind) || tombstones.rejects(&event.event.kind).is_some() {
                continue;
            }
            if let Ok(stamp) = stamp(row, DEFERRED_STAMP) {
                newest = Some((candidate.id, stamp));
            }
            if event
                .event
                .primary_run_id()
                .is_some_and(|run| runs.contains(run))
            {
                visit(DeferredRow::Event(Box::new(event)));
            }
            continue;
        }
        // An id collected as committed may since have been deleted and reused by another row.
        let kind: Option<String> = row.get(KIND_COLUMN).ok();
        let subkind: Option<String> = row.get(SUBKIND_COLUMN).ok().flatten();
        if !kind.zip(subkind).is_some_and(|(kind, subkind)| {
            COMMITTED_SUBKINDS.contains(&(kind.as_str(), subkind.as_str()))
        }) {
            continue;
        }
        let run_id: Option<String> = row.get(RUN_ID_COLUMN).ok().flatten();
        if run_id
            .as_deref()
            .is_some_and(|run| tombstones.contains_run(run))
        {
            continue;
        }
        if let Ok(stamp) = stamp(row, DEFERRED_STAMP) {
            newest = Some((candidate.id, stamp));
        }
        if !run_id.as_deref().is_some_and(|run| runs.contains(run)) {
            continue;
        }
        match deferred_row(row) {
            Ok(event) if tombstones.rejects(&event.event.kind).is_some() => {}
            Ok(event) => visit(DeferredRow::Event(Box::new(event))),
            Err(reason) => visit(DeferredRow::Failed(failed_row(row, run_id, reason)?)),
        }
    }
    record_fetched(conn, fetched);
    Ok(newest)
}

/// The newest committed row of the log whose run is not deleted, from one index entry per
/// committed pair. Unbackfilled rows newer than that are decoded, newest first.
///
/// # Errors
/// Fails only when a query itself fails.
pub fn max_committed_id(conn: &Connection) -> Result<Option<(i64, EventStamp)>, StorageError> {
    let transaction = conn.unchecked_transaction()?;
    let mut newest: Option<(i64, EventStamp)> = None;
    {
        let mut statement = transaction.prepare_cached(NEWEST_COMMITTED_SQL)?;
        for (kind, subkind) in COMMITTED_SUBKINDS {
            let mut rows = statement.query(params![kind, subkind])?;
            if let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                if newest.as_ref().is_none_or(|(best, _)| id > *best) {
                    newest = Some((id, stamp(row, [1, 2, 3])?));
                }
            }
        }
    }
    if has_unbackfilled(&transaction)? {
        let tombstones = TombstoneSet::load(&transaction)?;
        let floor = newest.as_ref().map_or(i64::MIN, |(id, _)| *id);
        let mut ids = transaction.prepare_cached(NEWEST_UNCONVERTED_SQL)?;
        let mut fetch = transaction.prepare_cached(&fetch_sql())?;
        let mut candidates = ids.query([floor])?;
        let mut fetched = 0_u64;
        while let Some(candidate) = candidates.next()? {
            let mut rows = fetch.query([candidate.get::<_, i64>(0)?])?;
            let Some(row) = rows.next()? else { continue };
            fetched += 1;
            let Ok(event) = deferred_row(row) else {
                continue;
            };
            if is_committed(&event.event.kind) && tombstones.rejects(&event.event.kind).is_none() {
                newest = Some((event.id, stamp(row, DEFERRED_STAMP)?));
                break;
            }
        }
        record_fetched(&transaction, fetched);
    }
    Ok(newest)
}

/// The stamp of each of `ids` still stored, in the order given.
///
/// # Errors
/// Fails only when a query itself fails.
pub fn event_stamps(
    conn: &Connection,
    ids: &[i64],
) -> Result<Vec<(i64, EventStamp)>, StorageError> {
    let mut statement = conn.prepare_cached(STAMP_SQL)?;
    let mut stamps = Vec::with_capacity(ids.len());
    for &id in ids {
        let mut rows = statement.query([id])?;
        if let Some(row) = rows.next()? {
            stamps.push((id, stamp(row, [0, 1, 2])?));
        }
    }
    Ok(stamps)
}
