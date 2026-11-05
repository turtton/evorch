//! Bounded physical deletion of a confirmed family.
//!
//! Each job walks fixed phases with a cursor kept in `reclaim_jobs`. A step
//! deletes a bounded amount, advances the cursor and adds to the job's counters
//! in one IMMEDIATE transaction, so an interruption between steps repeats
//! nothing and loses nothing. Rows too large for a bounded step are listed in
//! `reclaim_oversized` for the exclusive startup phase.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::StorageError;
use crate::repo::thread_index::{OversizedBlock, purge_index_rows, purge_index_rows_with};
use crate::thread_index::PurgeTarget;
use crate::thread_retention::{
    FilesOutcome, OversizedProgress, RECLAIM_EVENT_BYTES, RECLAIM_EVENT_ROWS, RECLAIM_INDEX_ROWS,
    RECLAIM_INDEX_TEXT_BYTES, RECLAIM_MESSAGE_REFS, RECLAIM_MESSAGE_ROWS, RECLAIM_ROW_BUDGET,
    RECLAIM_ROWS, RECLAIM_SCAN_ROWS, ReclaimBudget, ReclaimJobMembers, ReclaimJobState,
    ReclaimProgress, ReclaimSummary,
};

/// Families whose pending reclaim job holds a cursor this build does not read (not valid
/// JSON, an unknown phase). `step_job` would start such a job over without saying so; the
/// conversion refuses them instead.
pub(crate) fn invalid_cursor_families(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT family_key, cursor_json FROM reclaim_jobs WHERE state = 'db_pending' \
         ORDER BY family_key",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut invalid = Vec::new();
    for row in rows {
        let (family, cursor) = row?;
        let readable = serde_json::from_str::<serde_json::Value>(&cursor).is_ok_and(|value| {
            value.is_object() && serde_json::from_value::<Cursor>(value).is_ok()
        });
        if cursor != "{}" && !readable {
            invalid.push(family);
        }
    }
    Ok(invalid)
}

/// The shape of `run_contexts` a reclaim works on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    /// The running format: a header holds its configuration and checkpoint list only.
    Current,
    /// A database the conversion command is converting: the new tables exist, and a
    /// header row still holds the whole conversation in `messages_json`, so its stored
    /// size (and the budget it spends) includes that column.
    Converting,
}

impl Layout {
    /// SQL added to a header's stored-size expression.
    const fn header_extra_bytes(self) -> &'static str {
        match self {
            Self::Current => "",
            Self::Converting => " + OCTET_LENGTH(messages_json)",
        }
    }
}

/// Empty cursor advances (a run without rows in a table) one transaction may make.
const MAX_EMPTY_ADVANCES: u32 = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    EventsRun,
    EventsThread,
    RunTables,
    Index,
    RunMessages,
    RunContexts,
    Tasks,
    ThreadTables,
    Finished,
}

/// Where a job stands. Runs and threads are visited in id order; `run` and
/// `thread` name the one being processed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    #[serde(default)]
    phase: Phase,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    thread: Option<String>,
    /// Table within the phase.
    #[serde(default)]
    table_idx: u32,
    /// Last event id deleted for the current run.
    #[serde(default)]
    after: i64,
    /// Last ledger or todo row scanned.
    #[serde(default)]
    scan_after: i64,
    /// Last task whose rows are gone, in the task phase's per-task steps.
    #[serde(default)]
    task: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Work {
    /// Rows deleted.
    rows: u64,
    /// Stored bytes of the deleted rows.
    bytes: u64,
    /// Rows changed without being deleted (request targets closed as gaps). They
    /// use the step's row budget and end the transaction, but are not reclaimed rows.
    updated: u64,
}

/// Run tables in deletion order: (table, predicate on `?1`). Ledger rows need a grant.
const RUN_TABLES: &[(&str, &str)] = &[
    ("run_ledger", "run_id = ?1"),
    ("host_input_receipts", "run_id = ?1"),
    ("review_evidence", "root_run_id = ?1 OR target_run_id = ?1"),
    (
        "review_lineage_edges",
        "child_run_id = ?1 OR parent_run_id = ?1",
    ),
    ("assistant_references", "source_run_id = ?1"),
    (
        "user_question_links",
        "run_id = ?1 OR question_id IN (SELECT id FROM user_questions WHERE run_id = ?1)",
    ),
    ("user_questions", "run_id = ?1"),
    // The message rows name the header in a RESTRICT foreign key: they go first.
    ("run_message_provenance", "run_id = ?1"),
    ("run_context_provenance", "run_id = ?1"),
];

fn now_ns() -> i64 {
    crate::system_time_to_ns(std::time::SystemTime::now()).unwrap_or(0)
}

fn next_id(
    conn: &Connection,
    table: &str,
    column: &str,
    family: &str,
    after: &str,
) -> Result<Option<String>, StorageError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {column} FROM {table} WHERE family_key = ?1 AND {column} > ?2 \
                 ORDER BY {column} LIMIT 1"
            ),
            params![family, after],
            |row| row.get(0),
        )
        .optional()?)
}

fn next_run(conn: &Connection, family: &str, after: &str) -> Result<Option<String>, StorageError> {
    next_id(conn, "run_tombstones", "run_id", family, after)
}

fn next_thread(
    conn: &Connection,
    family: &str,
    after: &str,
) -> Result<Option<String>, StorageError> {
    next_id(conn, "thread_tombstones", "thread_id", family, after)
}

/// Moves the cursor to the start of `phase`, skipping a phase that has nothing
/// to visit.
fn enter(
    conn: &Connection,
    family: &str,
    cursor: &mut Cursor,
    mut phase: Phase,
) -> Result<(), StorageError> {
    loop {
        *cursor = Cursor {
            phase,
            ..Cursor::default()
        };
        match phase {
            Phase::EventsRun
            | Phase::RunTables
            | Phase::Index
            | Phase::RunMessages
            | Phase::RunContexts => {
                cursor.run = next_run(conn, family, "")?;
                if cursor.run.is_some() {
                    return Ok(());
                }
            }
            Phase::ThreadTables => {
                cursor.thread = next_thread(conn, family, "")?;
                if cursor.thread.is_some() {
                    return Ok(());
                }
            }
            Phase::EventsThread | Phase::Tasks | Phase::Finished => return Ok(()),
        }
        phase = match phase {
            Phase::EventsRun => Phase::EventsThread,
            Phase::EventsThread => Phase::RunTables,
            Phase::RunTables => Phase::Index,
            Phase::Index => Phase::RunMessages,
            Phase::RunMessages => Phase::RunContexts,
            Phase::RunContexts => Phase::Tasks,
            Phase::Tasks => Phase::ThreadTables,
            Phase::ThreadTables | Phase::Finished => Phase::Finished,
        };
    }
}

fn next_phase(phase: Phase) -> Phase {
    match phase {
        Phase::EventsRun => Phase::EventsThread,
        Phase::EventsThread => Phase::RunTables,
        Phase::RunTables => Phase::Index,
        Phase::Index => Phase::RunMessages,
        Phase::RunMessages => Phase::RunContexts,
        Phase::RunContexts => Phase::Tasks,
        Phase::Tasks => Phase::ThreadTables,
        Phase::ThreadTables | Phase::Finished => Phase::Finished,
    }
}

/// Advance to the next run of a per-run phase, or to the next phase.
fn advance_run(conn: &Connection, family: &str, cursor: &mut Cursor) -> Result<(), StorageError> {
    let current = cursor.run.clone().unwrap_or_default();
    match next_run(conn, family, &current)? {
        Some(run) => {
            cursor.run = Some(run);
            cursor.table_idx = 0;
            cursor.after = 0;
        }
        None => {
            let phase = next_phase(cursor.phase);
            enter(conn, family, cursor, phase)?;
        }
    }
    Ok(())
}

fn json_ids<T: Serialize>(ids: &[T]) -> Result<String, StorageError> {
    serde_json::to_string(ids).map_err(|error| StorageError::Serialization(error.to_string()))
}

fn register_oversized(
    conn: &Connection,
    family: &str,
    table: &str,
    key: &str,
    bytes: u64,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT OR REPLACE INTO reclaim_oversized(family_key, tbl, key, stored_bytes) \
         VALUES (?1, ?2, ?3, ?4)",
        params![family, table, key, bytes],
    )?;
    Ok(())
}

/// One unit of work of the cursor's phase. Returns what it deleted; an empty
/// result means the cursor only advanced.
fn do_phase(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
    layout: Layout,
) -> Result<Work, StorageError> {
    match cursor.phase {
        Phase::EventsRun => events_run(tx, family, cursor),
        Phase::EventsThread => events_thread(tx, family, cursor),
        Phase::RunTables => run_tables(tx, family, cursor),
        Phase::Index => index(tx, family, cursor),
        Phase::RunMessages => run_messages(tx, family, cursor),
        Phase::RunContexts => run_contexts(tx, family, cursor, layout),
        Phase::Tasks => tasks(tx, family, cursor),
        Phase::ThreadTables => thread_tables(tx, family, cursor),
        Phase::Finished => Ok(Work::default()),
    }
}

fn events_run(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
) -> Result<Work, StorageError> {
    let Some(run) = cursor.run.clone() else {
        enter(tx, family, cursor, Phase::EventsThread)?;
        return Ok(Work::default());
    };
    let mut ids = Vec::new();
    let mut bytes = 0_u64;
    let mut refs = 0_u64;
    let mut oversized: Option<(i64, u64)> = None;
    {
        // The stored payload and the reference rows the cascade deletes are the work of a
        // step. The physical bytes of the blobs are the collector's budget, and the
        // accounting charge (`blob_bytes`) is neither.
        let mut statement = tx.prepare_cached(
            "SELECT id, OCTET_LENGTH(payload), \
             (SELECT COUNT(*) FROM (SELECT 1 FROM content_blob_refs_event \
                                    WHERE event_id = events.id LIMIT ?4)) FROM events \
             WHERE run_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
        )?;
        let limit = i64::try_from(RECLAIM_EVENT_ROWS).unwrap_or(i64::MAX);
        let mut rows =
            statement.query(params![run, cursor.after, limit, RECLAIM_MESSAGE_REFS + 1])?;
        while let Some(row) = rows.next()? {
            let size = row.get::<_, u64>(1)?;
            let count = row.get::<_, u64>(2)?;
            let too_big = size > RECLAIM_EVENT_BYTES || count > RECLAIM_MESSAGE_REFS;
            if ids.is_empty() && too_big {
                oversized = Some((row.get::<_, i64>(0)?, size));
                break;
            }
            if too_big
                || bytes.saturating_add(size) > RECLAIM_EVENT_BYTES
                || refs.saturating_add(count) > RECLAIM_MESSAGE_REFS
            {
                break;
            }
            ids.push(row.get::<_, i64>(0)?);
            bytes = bytes.saturating_add(size);
            refs += count;
        }
    }
    if let Some((id, size)) = oversized {
        // A single row above the step's budget waits for the exclusive phase.
        register_oversized(tx, family, "events", &id.to_string(), size)?;
        cursor.after = id;
        return Ok(Work::default());
    }
    let Some(&last) = ids.last() else {
        advance_run(tx, family, cursor)?;
        return Ok(Work::default());
    };
    tx.execute(
        "DELETE FROM events WHERE id IN (SELECT value FROM json_each(?1))",
        [json_ids(&ids)?],
    )?;
    cursor.after = last;
    Ok(Work {
        rows: ids.len() as u64,
        bytes,
        ..Work::default()
    })
}

/// The (kind, subkind) pairs whose rows name a thread but no run.
fn thread_event_kinds() -> impl Iterator<Item = (&'static str, &'static str)> {
    std::iter::once(("Orchestrator", "ThreadTodoUpdated")).chain(
        event_bus::PullRequestEvent::SUBKINDS
            .iter()
            .map(|subkind| ("PullRequest", *subkind)),
    )
}

/// Thread todo snapshots and pull request changes carry no run: find them by the thread
/// they name, scanning a bounded number of rows per step.
///
/// Each (kind, subkind) is read through `idx_events_kind_subkind_id` in ID order, at most a
/// step's rows however long the history is, and the branches are merged here. One query
/// over all of them would sort in SQLite or scan every row after the cursor.
fn events_thread(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
) -> Result<Work, StorageError> {
    let limit = i64::try_from(RECLAIM_SCAN_ROWS).unwrap_or(i64::MAX);
    let mut seen: Vec<(i64, u64, bool)> = Vec::new();
    {
        let mut statement = tx.prepare_cached(
            "SELECT id, OCTET_LENGTH(payload), \
             EXISTS(SELECT 1 FROM thread_tombstones \
                    WHERE thread_id = COALESCE(\
                            json_extract(payload, '$.payload.payload.snapshot.thread_id'), \
                            json_extract(payload, '$.payload.payload.thread_id')) \
                      AND family_key = ?5) FROM events \
             WHERE kind = ?3 AND subkind = ?4 \
             AND run_id IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
        )?;
        for (kind, subkind) in thread_event_kinds() {
            // A point lookup per scanned row, so a family with many threads costs
            // no more than a small one.
            let mut rows =
                statement.query(params![cursor.scan_after, limit, kind, subkind, family])?;
            while let Some(row) = rows.next()? {
                seen.push((row.get(0)?, row.get(1)?, row.get(2)?));
            }
        }
    }
    seen.sort_unstable_by_key(|(id, _, _)| *id);
    seen.truncate(RECLAIM_SCAN_ROWS);
    let scanned = seen.len();
    let mut last = cursor.scan_after;
    let mut ids = Vec::new();
    let mut bytes = 0_u64;
    for (id, size, dead) in seen {
        last = id;
        if dead {
            ids.push(id);
            bytes = bytes.saturating_add(size);
        }
    }
    if !ids.is_empty() {
        tx.execute(
            "DELETE FROM events WHERE id IN (SELECT value FROM json_each(?1))",
            [json_ids(&ids)?],
        )?;
    }
    cursor.scan_after = last;
    if scanned < RECLAIM_SCAN_ROWS {
        enter(tx, family, cursor, Phase::RunTables)?;
    }
    Ok(Work {
        rows: ids.len() as u64,
        bytes,
        ..Work::default()
    })
}

fn run_tables(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
) -> Result<Work, StorageError> {
    let Some(run) = cursor.run.clone() else {
        enter(tx, family, cursor, Phase::Index)?;
        return Ok(Work::default());
    };
    let Some(&(table, predicate)) = RUN_TABLES.get(cursor.table_idx as usize) else {
        advance_run(tx, family, cursor)?;
        return Ok(Work::default());
    };
    if table == "run_message_provenance" {
        return provenance_entries(tx, family, cursor, &run);
    }
    if table == "run_context_provenance" {
        // The header goes after its entries (RESTRICT): an entry left for the exclusive
        // phase keeps it, and the header is registered so that phase works the entries
        // off before it.
        let entries_left: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_message_provenance WHERE run_id = ?1)",
            [&run],
            |row| row.get(0),
        )?;
        if entries_left {
            let stored: u64 = tx
                .query_row(
                    "SELECT COALESCE(OCTET_LENGTH(evidence_json), 0) \
                     FROM run_context_provenance WHERE run_id = ?1",
                    [&run],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            register_oversized(tx, family, "run_context_provenance", &run, stored)?;
            cursor.table_idx += 1;
            return Ok(Work::default());
        }
    }
    if table == "run_ledger" {
        tx.execute(
            "INSERT OR IGNORE INTO run_ledger_delete_grants(run_id) VALUES (?1)",
            [&run],
        )?;
    }
    let limit = i64::try_from(RECLAIM_ROWS).unwrap_or(i64::MAX);
    let deleted = tx.execute(
        &format!(
            "DELETE FROM {table} WHERE rowid IN \
             (SELECT rowid FROM {table} WHERE {predicate} LIMIT ?2)"
        ),
        params![run, limit],
    )?;
    if table == "run_ledger" {
        tx.execute(
            "DELETE FROM run_ledger_delete_grants WHERE run_id = ?1",
            [&run],
        )?;
    }
    if deleted == 0 {
        cursor.table_idx += 1;
    }
    Ok(Work {
        rows: deleted as u64,
        bytes: 0,
        ..Work::default()
    })
}

/// Deletes the run's provenance entries in bounded batches (`cursor.after` is the next
/// `map_index` to look at): at most [`RECLAIM_ROWS`] rows and [`RECLAIM_ROW_BUDGET`]
/// stored bytes per transaction. An entry above the budget alone is registered for the
/// exclusive phase and skipped; the header then waits for it.
fn provenance_entries(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
    run: &str,
) -> Result<Work, StorageError> {
    let rows: Vec<(i64, i64, u64)> = {
        let mut statement = tx.prepare_cached(
            "SELECT id, map_index, entry_len FROM run_message_provenance \
             WHERE run_id = ?1 AND map_index >= ?2 ORDER BY map_index LIMIT ?3",
        )?;
        let limit = i64::try_from(RECLAIM_ROWS).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![run, cursor.after, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    if rows.is_empty() {
        cursor.table_idx += 1;
        cursor.after = 0;
        return Ok(Work::default());
    }
    let mut ids = Vec::new();
    let mut bytes = 0_u64;
    let mut next = cursor.after;
    for (id, map_index, size) in rows {
        let too_big = size > RECLAIM_ROW_BUDGET;
        if ids.is_empty() && too_big {
            register_oversized(tx, family, "run_message_provenance", &id.to_string(), size)?;
            cursor.after = map_index + 1;
            return Ok(Work::default());
        }
        if too_big || bytes.saturating_add(size) > RECLAIM_ROW_BUDGET {
            break;
        }
        ids.push(id);
        bytes += size;
        next = map_index + 1;
    }
    tx.execute(
        "DELETE FROM run_message_provenance WHERE id IN (SELECT value FROM json_each(?1))",
        [json_ids(&ids)?],
    )?;
    cursor.after = next;
    Ok(Work {
        rows: ids.len() as u64,
        bytes,
        ..Work::default()
    })
}

fn index(tx: &Transaction<'_>, family: &str, cursor: &mut Cursor) -> Result<Work, StorageError> {
    let Some(run) = cursor.run.clone() else {
        enter(tx, family, cursor, Phase::RunMessages)?;
        return Ok(Work::default());
    };
    let progress = purge_index_rows(
        tx,
        &run,
        PurgeTarget::Tombstoned,
        RECLAIM_INDEX_ROWS,
        RECLAIM_INDEX_TEXT_BYTES,
    )?;
    if progress.oversized {
        // The size of the block that stopped the step is enough to order the
        // registration; summing the run's blocks would read all of them.
        register_oversized(
            tx,
            family,
            "conversation_blocks",
            &run,
            progress.oversized_bytes,
        )?;
        advance_run(tx, family, cursor)?;
    } else if !progress.remaining {
        advance_run(tx, family, cursor)?;
    }
    Ok(Work {
        rows: progress.rows as u64,
        bytes: progress.text_bytes,
        updated: progress.gapped as u64,
    })
}

/// Stored skeleton bytes and referenced blobs (capped just above `limit`) of a message.
fn message_cost(tx: &Connection, id: i64, size: u64) -> Result<(u64, u64), StorageError> {
    let refs: u64 = tx
        .prepare_cached(
            "SELECT COUNT(*) FROM (SELECT 1 FROM content_blob_refs_message \
             WHERE message_id = ?1 LIMIT ?2)",
        )?
        .query_row(params![id, RECLAIM_MESSAGE_REFS + 1], |row| row.get(0))?;
    Ok((size, refs))
}

/// Deletes the run's messages in bounded batches (`cursor.after` is the next ordinal to look at): at most [`RECLAIM_MESSAGE_ROWS`] rows,
/// [`RECLAIM_ROW_BUDGET`] stored bytes and [`RECLAIM_MESSAGE_REFS`] reference rows (which
/// the cascade deletes and queues) per transaction. A message above either limit alone
/// is registered for the exclusive phase and skipped; the header then waits for it.
fn run_messages(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
) -> Result<Work, StorageError> {
    let Some(run) = cursor.run.clone() else {
        enter(tx, family, cursor, Phase::RunContexts)?;
        return Ok(Work::default());
    };
    let rows: Vec<(i64, i64, u64)> = {
        let mut statement = tx.prepare_cached(
            "SELECT id, ordinal, OCTET_LENGTH(skeleton) FROM run_messages \
             WHERE run_id = ?1 AND ordinal >= ?2 ORDER BY ordinal LIMIT ?3",
        )?;
        let limit = i64::try_from(RECLAIM_MESSAGE_ROWS).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![run, cursor.after, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    if rows.is_empty() {
        advance_run(tx, family, cursor)?;
        return Ok(Work::default());
    }
    let mut ids = Vec::new();
    let mut bytes = 0_u64;
    let mut refs = 0_u64;
    let mut next = cursor.after;
    for (id, ordinal, size) in rows {
        let (size, count) = message_cost(tx, id, size)?;
        let too_big = size > RECLAIM_ROW_BUDGET || count > RECLAIM_MESSAGE_REFS;
        if ids.is_empty() && too_big {
            register_oversized(tx, family, "run_messages", &id.to_string(), size)?;
            cursor.after = ordinal + 1;
            return Ok(Work::default());
        }
        if too_big
            || bytes.saturating_add(size) > RECLAIM_ROW_BUDGET
            || refs.saturating_add(count) > RECLAIM_MESSAGE_REFS
        {
            break;
        }
        ids.push(id);
        bytes += size;
        refs += count;
        next = ordinal + 1;
    }
    tx.execute(
        "DELETE FROM run_messages WHERE id IN (SELECT value FROM json_each(?1))",
        [json_ids(&ids)?],
    )?;
    cursor.after = next;
    Ok(Work {
        rows: ids.len() as u64,
        bytes,
        ..Work::default()
    })
}

fn run_contexts(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
    layout: Layout,
) -> Result<Work, StorageError> {
    let Some(run) = cursor.run.clone() else {
        enter(tx, family, cursor, Phase::Tasks)?;
        return Ok(Work::default());
    };
    let stored: Option<u64> = tx
        .query_row(
            &format!(
                "SELECT OCTET_LENGTH(config_json) + OCTET_LENGTH(checkpoints_json){} \
                 FROM run_contexts WHERE run_id = ?1",
                layout.header_extra_bytes()
            ),
            [&run],
            |row| row.get(0),
        )
        .optional()?;
    // A message that was left for the exclusive phase keeps its header: deleting the
    // header first would stop at the foreign key, and a header that is skipped here
    // is registered, so the exclusive phase works the messages off before it.
    let children_left: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_messages WHERE run_id = ?1)",
        [&run],
        |row| row.get(0),
    )?;
    let header_refs: u64 = tx.query_row(
        "SELECT COUNT(*) FROM (SELECT 1 FROM content_blob_refs_header WHERE run_id = ?1 LIMIT ?2)",
        params![run, RECLAIM_MESSAGE_REFS + 1],
        |row| row.get(0),
    )?;
    let mut work = Work::default();
    match stored {
        None => {}
        Some(bytes)
            if bytes > RECLAIM_ROW_BUDGET
                || children_left
                || header_refs > RECLAIM_MESSAGE_REFS =>
        {
            register_oversized(tx, family, "run_contexts", &run, bytes)?;
        }
        Some(bytes) => {
            tx.execute("DELETE FROM run_contexts WHERE run_id = ?1", [&run])?;
            work = Work {
                rows: 1,
                bytes,
                ..Work::default()
            };
        }
    }
    advance_run(tx, family, cursor)?;
    Ok(work)
}

/// The family's tombstoned task after `after`, in id order.
fn next_task(
    conn: &Connection,
    family: &str,
    after: Option<&str>,
) -> Result<Option<String>, StorageError> {
    next_id(
        conn,
        "task_tombstones",
        "task_id",
        family,
        after.unwrap_or(""),
    )
}

/// Queue ledger rows name tasks only inside their JSON payload, so they are
/// scanned by sequence number (each row costs point lookups into the family's
/// task tombstones, never a read of all of them); then links and tasks, found
/// through the tombstones one id after another with a cursor, so a task whose
/// rows are gone is never visited again.
fn tasks(tx: &Transaction<'_>, family: &str, cursor: &mut Cursor) -> Result<Work, StorageError> {
    match cursor.table_idx {
        0 => {
            let mut scanned = 0_usize;
            let mut last = cursor.scan_after;
            let mut seqs = Vec::new();
            {
                let named = |path: &str| {
                    format!(
                        "EXISTS(SELECT 1 FROM task_tombstones \
                         WHERE task_id = json_extract(payload, '{path}') AND family_key = ?3)"
                    )
                };
                let mut statement = tx.prepare_cached(&format!(
                    "SELECT seq, {} OR {} OR {} FROM task_queue_ledger \
                     WHERE seq > ?1 ORDER BY seq LIMIT ?2",
                    named("$.id"),
                    named("$.blocker"),
                    named("$.blocked"),
                ))?;
                let limit = i64::try_from(RECLAIM_SCAN_ROWS).unwrap_or(i64::MAX);
                let mut rows = statement.query(params![cursor.scan_after, limit, family])?;
                while let Some(row) = rows.next()? {
                    scanned += 1;
                    last = row.get(0)?;
                    if row.get::<_, bool>(1)? {
                        seqs.push(last);
                    }
                }
            }
            for seq in &seqs {
                tx.execute(
                    "INSERT OR IGNORE INTO task_queue_delete_grants(seq) VALUES (?1)",
                    [seq],
                )?;
                tx.execute("DELETE FROM task_queue_ledger WHERE seq = ?1", [seq])?;
                tx.execute("DELETE FROM task_queue_delete_grants WHERE seq = ?1", [seq])?;
            }
            cursor.scan_after = last;
            if scanned < RECLAIM_SCAN_ROWS {
                cursor.table_idx = 1;
                cursor.task = None;
            }
            Ok(Work {
                rows: seqs.len() as u64,
                bytes: 0,
                ..Work::default()
            })
        }
        1 => {
            // Links, task after task, until the step has deleted its row budget or
            // looked at its share of tasks.
            let mut deleted = 0_usize;
            for _ in 0..RECLAIM_ROWS {
                let Some(task) = next_task(tx, family, cursor.task.as_deref())? else {
                    cursor.table_idx = 2;
                    cursor.task = None;
                    break;
                };
                let room = i64::try_from(RECLAIM_ROWS - deleted).unwrap_or(i64::MAX);
                let removed = tx.execute(
                    "DELETE FROM task_links WHERE rowid IN (SELECT rowid FROM task_links \
                     WHERE blocker_id = ?1 OR blocked_id = ?1 LIMIT ?2)",
                    params![task, room],
                )?;
                deleted += removed;
                if removed < usize::try_from(room).unwrap_or(usize::MAX) {
                    cursor.task = Some(task);
                }
                if deleted >= RECLAIM_ROWS {
                    break;
                }
            }
            Ok(Work {
                rows: deleted as u64,
                bytes: 0,
                ..Work::default()
            })
        }
        2 => {
            let limit = i64::try_from(RECLAIM_ROWS).unwrap_or(i64::MAX);
            let ids: Vec<String> = {
                let mut statement = tx.prepare_cached(
                    "SELECT task_id FROM task_tombstones \
                     WHERE family_key = ?1 AND task_id > ?2 ORDER BY task_id LIMIT ?3",
                )?;
                statement
                    .query_map(
                        params![family, cursor.task.as_deref().unwrap_or(""), limit],
                        |row| row.get(0),
                    )?
                    .collect::<Result<_, _>>()?
            };
            let Some(last) = ids.last() else {
                enter(tx, family, cursor, Phase::ThreadTables)?;
                return Ok(Work::default());
            };
            cursor.task = Some(last.clone());
            let deleted = tx.execute(
                "DELETE FROM tasks WHERE id IN (SELECT value FROM json_each(?1))",
                [json_ids(&ids)?],
            )?;
            Ok(Work {
                rows: deleted as u64,
                bytes: 0,
                ..Work::default()
            })
        }
        _ => {
            enter(tx, family, cursor, Phase::ThreadTables)?;
            Ok(Work::default())
        }
    }
}

/// What one bounded look at a thread's captured untracked files found.
enum Untracked {
    /// A batch was deleted.
    Taken(Work),
    /// The next file alone is above the step's byte budget (its size).
    Oversized(u64),
    /// No file is left.
    None,
}

/// Deletes up to [`RECLAIM_ROWS`] captured files of `thread` whose sizes fit
/// [`RECLAIM_ROW_BUDGET`] together. A first file above the budget is left
/// alone unless `take_oversized` (the exclusive phase), which deletes it by
/// itself. At most [`RECLAIM_ROWS`] rows are read, whatever their number.
fn take_untracked(
    tx: &Transaction<'_>,
    thread: &str,
    take_oversized: bool,
) -> Result<Untracked, StorageError> {
    let mut rowids = Vec::new();
    let mut bytes = 0_u64;
    {
        let mut statement = tx.prepare_cached(
            "SELECT rowid, OCTET_LENGTH(content) FROM thread_start_untracked \
             WHERE thread_id = ?1 LIMIT ?2",
        )?;
        let limit = i64::try_from(RECLAIM_ROWS).unwrap_or(i64::MAX);
        let mut rows = statement.query(params![thread, limit])?;
        while let Some(row) = rows.next()? {
            let size = row.get::<_, u64>(1)?;
            if rowids.is_empty() && size > RECLAIM_ROW_BUDGET {
                if !take_oversized {
                    return Ok(Untracked::Oversized(size));
                }
                rowids.push(row.get::<_, i64>(0)?);
                bytes = size;
                break;
            }
            if !rowids.is_empty() && bytes.saturating_add(size) > RECLAIM_ROW_BUDGET {
                break;
            }
            rowids.push(row.get::<_, i64>(0)?);
            bytes = bytes.saturating_add(size);
        }
    }
    if rowids.is_empty() {
        return Ok(Untracked::None);
    }
    tx.execute(
        "DELETE FROM thread_start_untracked WHERE rowid IN (SELECT value FROM json_each(?1))",
        [json_ids(&rowids)?],
    )?;
    Ok(Untracked::Taken(Work {
        rows: rowids.len() as u64,
        bytes,
        ..Work::default()
    }))
}

/// Moves to the next thread of the job, or finishes the phase.
fn advance_thread(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
    thread: &str,
) -> Result<(), StorageError> {
    match next_thread(tx, family, thread)? {
        Some(next) => {
            cursor.thread = Some(next);
            cursor.table_idx = 0;
        }
        None => enter(tx, family, cursor, Phase::Finished)?,
    }
    Ok(())
}

/// The tables keyed by thread, one table per `table_idx`: 0 the lane ledger rows, 1 the
/// pull request rows, 2 the captured untracked files, 3 the start state. The lane and pull
/// request rows come first, so a capture above the step budget (left to the exclusive
/// phase) never makes a thread skip them.
fn thread_tables(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
) -> Result<Work, StorageError> {
    let Some(thread) = cursor.thread.clone() else {
        enter(tx, family, cursor, Phase::Finished)?;
        return Ok(Work::default());
    };
    let mut work = Work::default();
    match cursor.table_idx {
        0 => {
            // A database the conversion command works on can still be at version 40: it
            // has no lane tables (and so no lanes) yet, whatever the layout of its runs.
            let lane_ledger: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'lanes')",
                [],
                |row| row.get(0),
            )?;
            if !lane_ledger {
                cursor.table_idx = 1;
            } else {
                work = take_lane_rows(tx, &thread)?;
                if work.rows == 0 {
                    cursor.table_idx = 1;
                }
            }
        }
        1 => {
            // The same holds for the pull request tables (migration 46).
            let pull_requests: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' \
                 AND name = 'thread_pull_requests')",
                [],
                |row| row.get(0),
            )?;
            if pull_requests {
                work = take_pull_request_rows(tx, &thread)?;
            }
            if work.rows == 0 {
                cursor.table_idx = 2;
            }
        }
        2 => match take_untracked(tx, &thread, false)? {
            Untracked::Taken(taken) => work = taken,
            // A captured file above the step's budget cannot be deleted by a bounded
            // step. The state row owns the untracked rows (ON DELETE CASCADE), so the
            // capture is registered once, by the size of that file, and deleted in
            // batches by the exclusive phase.
            Untracked::Oversized(size) => {
                register_oversized(tx, family, "thread_start_states", &thread, size)?;
                advance_thread(tx, family, cursor, &thread)?;
            }
            Untracked::None => cursor.table_idx = 3,
        },
        _ => {
            let stored: Option<u64> = tx
                .query_row(
                    "SELECT COALESCE(LENGTH(diff), 0) FROM thread_start_states WHERE thread_id = ?1",
                    [&thread],
                    |row| row.get(0),
                )
                .optional()?;
            match stored {
                None => {}
                Some(bytes) if bytes > RECLAIM_ROW_BUDGET => {
                    register_oversized(tx, family, "thread_start_states", &thread, bytes)?;
                }
                Some(bytes) => {
                    tx.execute(
                        "DELETE FROM thread_start_states WHERE thread_id = ?1",
                        [&thread],
                    )?;
                    work = Work {
                        rows: 1,
                        bytes,
                        ..Work::default()
                    };
                }
            }
            advance_thread(tx, family, cursor, &thread)?;
        }
    }
    Ok(work)
}

/// Works off the pull request rows of `thread`, at most [`RECLAIM_ROWS`] per call and in
/// this order: first the cached facts of the snapshots only this thread references are
/// cleared (the thread's own rows still name them), then its links, candidates,
/// inheritance row and lifecycle state are deleted. Returns no rows when nothing is left.
fn take_pull_request_rows(tx: &Transaction<'_>, thread: &str) -> Result<Work, StorageError> {
    let limit = i64::try_from(RECLAIM_ROWS).unwrap_or(i64::MAX);
    let cleared = super::super::pull_requests::snapshots::clear_unshared(tx, thread, limit)?;
    if cleared > 0 {
        return Ok(Work {
            rows: cleared,
            ..Work::default()
        });
    }
    for sql in [
        "DELETE FROM thread_pull_requests WHERE link_id IN \
         (SELECT link_id FROM thread_pull_requests WHERE thread_id = ?1 LIMIT ?2)",
        "DELETE FROM thread_pull_request_candidates WHERE candidate_id IN \
         (SELECT candidate_id FROM thread_pull_request_candidates WHERE thread_id = ?1 LIMIT ?2)",
        "DELETE FROM thread_pull_request_inheritance WHERE child_thread_id = ?1 AND ?2 > 0",
        "DELETE FROM thread_pull_request_state WHERE thread_id = ?1 AND ?2 > 0",
    ] {
        let deleted = tx.execute(sql, params![thread, limit])?;
        if deleted > 0 {
            return Ok(Work {
                rows: deleted as u64,
                ..Work::default()
            });
        }
    }
    Ok(Work::default())
}

/// Deletes up to [`RECLAIM_ROWS`] rows of the lane ledger of `thread`: reports first,
/// then delivery keys, then the lane row itself once nothing else is left. Returns no
/// rows when the thread has no lane (any more). None of these tables is fenced against
/// DELETE, so reclaim always progresses, and each step is bounded.
fn take_lane_rows(tx: &Transaction<'_>, thread: &str) -> Result<Work, StorageError> {
    let limit = i64::try_from(RECLAIM_ROWS).unwrap_or(i64::MAX);
    // New Phase 1c tables do not exist during older-format offline conversion.
    let phase2_tables: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='lane_stall_episodes')",
        [],
        |r| r.get(0),
    )?;
    if phase2_tables {
        for table in [
            "lane_forward_receipts",
            "lane_closes",
            "lane_verifications",
            "lane_stall_episodes",
        ] {
            let bytes_expr = if table == "lane_forward_receipts" {
                "OCTET_LENGTH(request_json)+OCTET_LENGTH(result_json)"
            } else if table == "lane_closes" {
                "OCTET_LENGTH(payload)+COALESCE(OCTET_LENGTH(pr_evidence_json),0)"
            } else {
                "OCTET_LENGTH(payload)"
            };
            let bytes:u64=tx.query_row(&format!("SELECT COALESCE(SUM(bytes),0) FROM (SELECT {bytes_expr} AS bytes FROM {table} WHERE lane_id IN (SELECT lane_id FROM lanes WHERE thread_id=?1) LIMIT ?2)"),params![thread,limit],|r|r.get(0))?;
            let deleted=tx.execute(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE lane_id IN (SELECT lane_id FROM lanes WHERE thread_id=?1) LIMIT ?2)"),params![thread,limit])?;
            if deleted > 0 {
                return Ok(Work {
                    rows: deleted as u64,
                    bytes,
                    ..Work::default()
                });
            }
        }
    }
    let delivery_tables: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='lane_notifications')",
        [],
        |r| r.get(0),
    )?;
    if delivery_tables {
        for (table, predicate, bytes_expr) in [
            (
                "lane_question_routes",
                "requester_thread_id=?1 OR responder_thread_id=?1 OR lane_id IN (SELECT lane_id FROM lanes WHERE thread_id=?1)",
                "OCTET_LENGTH(payload)+COALESCE(OCTET_LENGTH(operation_json),0)",
            ),
            (
                "lane_notifications",
                "source_thread_id=?1 OR destination_thread_id=?1 OR lane_id IN (SELECT lane_id FROM lanes WHERE thread_id=?1)",
                "OCTET_LENGTH(payload)",
            ),
            (
                "lane_delivery_destinations",
                "thread_id=?1",
                "COALESCE(OCTET_LENGTH(owner_json),0)",
            ),
        ] {
            let (rows, bytes): (u64,u64)=tx.query_row(&format!("SELECT COUNT(*),COALESCE(SUM(bytes),0) FROM (SELECT {bytes_expr} AS bytes FROM {table} WHERE {predicate} LIMIT ?2)"),params![thread,limit],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if rows > 0 {
                tx.execute(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE {predicate} LIMIT ?2)"),params![thread,limit])?;
                return Ok(Work {
                    rows,
                    bytes,
                    ..Work::default()
                });
            }
        }
    }
    let (rows, bytes): (u64, u64) = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(OCTET_LENGTH(summary) + OCTET_LENGTH(artifact_refs_json) \
            + OCTET_LENGTH(question_refs_json)), 0) FROM \
         (SELECT summary, artifact_refs_json, question_refs_json FROM lane_reports \
          WHERE lane_id IN (SELECT lane_id FROM lanes WHERE thread_id = ?1) LIMIT ?2)",
        params![thread, limit],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if rows > 0 {
        tx.execute(
            "DELETE FROM lane_reports WHERE (lane_id, report_seq) IN \
             (SELECT lane_id, report_seq FROM lane_reports \
             WHERE lane_id IN (SELECT lane_id FROM lanes WHERE thread_id = ?1) LIMIT ?2)",
            params![thread, limit],
        )?;
        return Ok(Work {
            rows,
            bytes,
            ..Work::default()
        });
    }
    let keys = tx.execute(
        "DELETE FROM lane_delivery_keys WHERE (lane_id, management_revision, event_kind, dedupe_key) IN \
         (SELECT lane_id, management_revision, event_kind, dedupe_key FROM lane_delivery_keys \
          WHERE lane_id IN (SELECT lane_id FROM lanes WHERE thread_id = ?1) LIMIT ?2)",
        params![thread, limit],
    )?;
    if keys > 0 {
        return Ok(Work {
            rows: keys as u64,
            ..Work::default()
        });
    }
    let deleted = tx.execute("DELETE FROM lanes WHERE thread_id = ?1", [thread])?;
    Ok(Work {
        rows: deleted as u64,
        ..Work::default()
    })
}

/// One transaction of one job. Returns what it deleted.
fn step_job(conn: &Connection, family: &str, layout: Layout) -> Result<(Work, bool), StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let state: Option<(String, String)> = tx
        .query_row(
            "SELECT state, cursor_json FROM reclaim_jobs WHERE family_key = ?1",
            [family],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state, cursor_json)) = state else {
        return Ok((Work::default(), true));
    };
    if state != "db_pending" {
        return Ok((Work::default(), true));
    }
    let mut cursor: Cursor = serde_json::from_str(&cursor_json).unwrap_or_default();
    if cursor == Cursor::default() && cursor_json == "{}" {
        enter(&tx, family, &mut cursor, Phase::EventsRun)?;
    }
    let mut work = Work::default();
    let mut empty = 0;
    while cursor.phase != Phase::Finished {
        // A scan reads up to RECLAIM_SCAN_ROWS rows: never more than one per transaction.
        let scanning = matches!(cursor.phase, Phase::EventsThread | Phase::Tasks);
        let step = do_phase(&tx, family, &mut cursor, layout)?;
        work.rows += step.rows;
        work.bytes += step.bytes;
        if scanning || step.rows > 0 || step.bytes > 0 || step.updated > 0 {
            break;
        }
        empty += 1;
        if empty >= MAX_EMPTY_ADVANCES {
            break;
        }
    }
    let mut finished = false;
    let mut new_state = "db_pending";
    if cursor.phase == Phase::Finished {
        let oversized: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM reclaim_oversized WHERE family_key = ?1)",
            [family],
            |row| row.get(0),
        )?;
        if !oversized {
            new_state = "db_done";
        }
        finished = true;
    }
    tx.execute(
        "UPDATE reclaim_jobs SET state = ?2, cursor_json = ?3, updated_at_ns = ?4, \
         reclaimed_rows = reclaimed_rows + ?5, reclaimed_stored_bytes = reclaimed_stored_bytes + ?6, \
         waiting_exclusive = ?7 WHERE family_key = ?1",
        params![
            family,
            new_state,
            serde_json::to_string(&cursor)
                .map_err(|error| StorageError::Serialization(error.to_string()))?,
            now_ns(),
            i64::try_from(work.rows).unwrap_or(i64::MAX),
            i64::try_from(work.bytes).unwrap_or(i64::MAX),
            // Only the exclusive phase can move a job on from here.
            i64::from(finished && new_state == "db_pending"),
        ],
    )?;
    tx.commit()?;
    Ok((work, finished))
}

/// Registrations one `reclaim_oversized_step` call looks at, whatever its budget.
const MAX_OVERSIZED_PER_CALL: u32 = 256;

/// Reported counts stop here, so reading one never follows how many jobs or
/// registrations there are.
const COUNT_CAP: u64 = 1_000;

/// `COUNT(*)` of `FROM <source>`, saturating at [`COUNT_CAP`].
fn count(conn: &Connection, source: &str) -> Result<u64, StorageError> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM (SELECT 1 {source} LIMIT {COUNT_CAP})"),
        [],
        |row| row.get::<_, u64>(0),
    )?)
}

/// Runs up to `budget.max_transactions` reclaim transactions over the pending
/// jobs in family order. Reads only when nothing is pending.
pub(crate) fn reclaim_step(
    conn: &Connection,
    budget: ReclaimBudget,
) -> Result<ReclaimProgress, StorageError> {
    reclaim_step_in(conn, budget, Layout::Current)
}

/// [`reclaim_step`] for a database in either run layout.
pub(crate) fn reclaim_step_in(
    conn: &Connection,
    budget: ReclaimBudget,
    layout: Layout,
) -> Result<ReclaimProgress, StorageError> {
    let mut progress = ReclaimProgress::default();
    let families: Vec<String> = {
        // Every job takes at least one transaction, so the budget bounds the list.
        let mut statement = conn.prepare_cached(
            "SELECT family_key FROM reclaim_jobs INDEXED BY idx_reclaim_jobs_runnable \
             WHERE state = 'db_pending' AND waiting_exclusive = 0 \
             ORDER BY family_key LIMIT ?1",
        )?;
        statement
            .query_map([i64::from(budget.max_transactions)], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<_, _>>()?
    };
    'jobs: for family in &families {
        loop {
            if progress.transactions >= budget.max_transactions {
                break 'jobs;
            }
            let (work, finished) = match step_job(conn, family, layout) {
                Ok(result) => result,
                Err(error) => {
                    progress.last_error = Some(error.to_string());
                    break 'jobs;
                }
            };
            progress.transactions += 1;
            progress.rows += work.rows;
            progress.stored_bytes += work.bytes;
            if finished {
                break;
            }
        }
    }
    // A job whose bounded walk is over and which only waits for the exclusive
    // phase is not runnable: it takes no transaction and blocks no other job.
    progress.jobs_db_pending = count(
        conn,
        "FROM reclaim_jobs INDEXED BY idx_reclaim_jobs_runnable \
         WHERE state = 'db_pending' AND waiting_exclusive = 0",
    )?;
    progress.jobs_waiting_exclusive = count(
        conn,
        "FROM reclaim_jobs INDEXED BY idx_reclaim_jobs_waiting \
         WHERE state = 'db_pending' AND waiting_exclusive = 1",
    )?;
    progress.jobs_done = count(
        conn,
        "FROM reclaim_jobs WHERE state IN ('db_done', 'files_pending', 'done', 'files_failed')",
    )?;
    progress.oversized_pending = count(conn, "FROM reclaim_oversized")?;
    Ok(progress)
}

/// Deletes oversized rows smallest first, one bounded transaction each, until
/// about `budget_bytes` of stored data are gone. At least one row is deleted when
/// any is waiting. A registration that owns many rows (a run's index blocks, a
/// capture's files) is worked off in batches: it stays registered, and the next
/// call or loop pass continues it, so no transaction deletes more than one batch
/// or one oversized row. A failure leaves the registration and stops the step.
///
/// `rows` and `stored_bytes` count what was actually deleted. A call looks at no more
/// than [`MAX_OVERSIZED_PER_CALL`] registrations, so when its first ones are already
/// gone it can return `rows == 0` with `remaining > 0` and no error: callers must
/// repeat until `remaining == 0`, never until `rows == 0`.
pub(crate) fn reclaim_oversized_step(
    conn: &Connection,
    budget_bytes: u64,
) -> Result<OversizedProgress, StorageError> {
    reclaim_oversized_step_in(conn, budget_bytes, Layout::Current)
}

/// [`reclaim_oversized_step`] for a database in either run layout.
pub(crate) fn reclaim_oversized_step_in(
    conn: &Connection,
    budget_bytes: u64,
    layout: Layout,
) -> Result<OversizedProgress, StorageError> {
    let mut progress = OversizedProgress::default();
    let mut worked = false;
    // Registrations whose rows are already gone delete nothing; the cap keeps a call
    // from walking through any number of them.
    for _ in 0..MAX_OVERSIZED_PER_CALL {
        let next: Option<(String, String, String, u64)> = conn
            .query_row(
                "SELECT family_key, tbl, key, stored_bytes FROM reclaim_oversized \
                 ORDER BY stored_bytes, family_key, tbl, key LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((family, table, key, mut bytes)) = next else {
            break;
        };
        if table == "run_contexts" {
            // A header with messages left spends the size of the next message, not its own.
            let child: Option<u64> = conn
                .query_row(
                    "SELECT OCTET_LENGTH(skeleton) FROM run_messages \
                     WHERE run_id = ?1 ORDER BY ordinal LIMIT 1",
                    [&key],
                    |row| row.get(0),
                )
                .optional()?;
            bytes = child.unwrap_or(bytes);
        }
        if table == "run_context_provenance" {
            // Same for a provenance header with entries left.
            let child: Option<u64> = conn
                .query_row(
                    "SELECT entry_len FROM run_message_provenance \
                     WHERE run_id = ?1 ORDER BY map_index LIMIT 1",
                    [&key],
                    |row| row.get(0),
                )
                .optional()?;
            bytes = child.unwrap_or(bytes);
        }
        if worked && progress.stored_bytes.saturating_add(bytes) > budget_bytes {
            break;
        }
        match delete_oversized(conn, &family, &table, &key, layout) {
            Ok(work) => {
                worked |= work.rows > 0 || work.updated > 0;
                progress.rows += work.rows;
                progress.stored_bytes = progress.stored_bytes.saturating_add(work.bytes);
            }
            Err(error) => {
                progress.last_error = Some(error.to_string());
                break;
            }
        }
    }
    progress.remaining = count(conn, "FROM reclaim_oversized")?;
    Ok(progress)
}

/// One transaction of the exclusive phase for one registration. The registration
/// is dropped once nothing of it is left.
fn delete_oversized(
    conn: &Connection,
    family: &str,
    table: &str,
    key: &str,
    layout: Layout,
) -> Result<Work, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let (work, finished) = match table {
        "events" => {
            let id: i64 = key.parse().map_err(|_| {
                StorageError::Serialization(format!("bad oversized event id {key}"))
            })?;
            let row: Option<u64> = tx
                .query_row(
                    "SELECT OCTET_LENGTH(payload) FROM events WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            let mut work = Work::default();
            if let Some(size) = row {
                tx.execute("DELETE FROM events WHERE id = ?1", [id])?;
                work = Work {
                    rows: 1,
                    bytes: size,
                    ..Work::default()
                };
            }
            (work, true)
        }
        "conversation_blocks" => {
            // A deleted run's blocks (a reclaim job), or those of a run the archive
            // purge handed over. The latter is deleted only while its thread is
            // still archived and the run is still `evicted(archived)`: an
            // unarchive in between leaves everything and just drops the entry.
            let tombstoned: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)",
                [key],
                |row| row.get(0),
            )?;
            let target = if tombstoned {
                Some(PurgeTarget::Tombstoned)
            } else if crate::repo::thread_index::run_in_archived_thread(&tx, key)? {
                Some(PurgeTarget::Archived)
            } else {
                None
            };
            match target {
                Some(target) => {
                    let progress = purge_index_rows_with(
                        &tx,
                        key,
                        target,
                        RECLAIM_INDEX_ROWS,
                        RECLAIM_INDEX_TEXT_BYTES,
                        OversizedBlock::TakeAlone,
                    )?;
                    (
                        Work {
                            rows: progress.rows as u64,
                            bytes: progress.text_bytes,
                            updated: progress.gapped as u64,
                        },
                        !progress.remaining,
                    )
                }
                None => (Work::default(), true),
            }
        }
        "content_blobs" => {
            // A blob a tick handed over because of its size. It is deleted only if no
            // reference table names it now.
            let (rows, bytes) = crate::repo::content_blobs::delete_oversized(&tx, key)?;
            (
                Work {
                    rows,
                    bytes,
                    ..Work::default()
                },
                true,
            )
        }
        "run_messages" => {
            let id: i64 = key.parse().map_err(|_| {
                StorageError::Serialization(format!("bad oversized message id {key}"))
            })?;
            let size: Option<u64> = tx
                .query_row(
                    "SELECT OCTET_LENGTH(skeleton) FROM run_messages WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            let deleted = tx.execute("DELETE FROM run_messages WHERE id = ?1", [id])?;
            (
                Work {
                    rows: deleted as u64,
                    bytes: size.unwrap_or(0),
                    ..Work::default()
                },
                true,
            )
        }
        "run_message_provenance" => {
            let id: i64 = key.parse().map_err(|_| {
                StorageError::Serialization(format!("bad oversized provenance entry id {key}"))
            })?;
            let size: Option<u64> = tx
                .query_row(
                    "SELECT entry_len FROM run_message_provenance WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            let deleted = tx.execute("DELETE FROM run_message_provenance WHERE id = ?1", [id])?;
            (
                Work {
                    rows: deleted as u64,
                    bytes: size.unwrap_or(0),
                    ..Work::default()
                },
                true,
            )
        }
        "run_context_provenance" => {
            // The header goes last, like a run header: while an entry of the run is left,
            // one of them is deleted by this step (the header's registration stays).
            let child: Option<(i64, u64)> = tx
                .query_row(
                    "SELECT id, entry_len FROM run_message_provenance \
                     WHERE run_id = ?1 ORDER BY map_index LIMIT 1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((id, size)) = child {
                tx.execute("DELETE FROM run_message_provenance WHERE id = ?1", [id])?;
                tx.execute(
                    "DELETE FROM reclaim_oversized WHERE family_key = ?1 \
                     AND tbl = 'run_message_provenance' AND key = ?2",
                    params![family, id.to_string()],
                )?;
                (
                    Work {
                        rows: 1,
                        bytes: size,
                        ..Work::default()
                    },
                    false,
                )
            } else {
                let size: Option<u64> = tx
                    .query_row(
                        "SELECT COALESCE(OCTET_LENGTH(evidence_json), 0) \
                         FROM run_context_provenance WHERE run_id = ?1",
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?;
                let deleted = tx.execute(
                    "DELETE FROM run_context_provenance WHERE run_id = ?1",
                    [key],
                )?;
                (
                    Work {
                        rows: deleted as u64,
                        bytes: size.unwrap_or(0),
                        ..Work::default()
                    },
                    true,
                )
            }
        }
        "run_contexts" => {
            // The header goes last: while a message of the run is left, one of them is
            // deleted by this step (the header's registration stays), so the
            // foreign key never stops it and each step makes progress.
            let child: Option<(i64, u64)> = tx
                .query_row(
                    "SELECT id, OCTET_LENGTH(skeleton) FROM run_messages \
                     WHERE run_id = ?1 ORDER BY ordinal LIMIT 1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((id, size)) = child {
                tx.execute("DELETE FROM run_messages WHERE id = ?1", [id])?;
                tx.execute(
                    "DELETE FROM reclaim_oversized WHERE family_key = ?1 \
                     AND tbl = 'run_messages' AND key = ?2",
                    params![family, id.to_string()],
                )?;
                (
                    Work {
                        rows: 1,
                        bytes: size,
                        ..Work::default()
                    },
                    false,
                )
            } else {
                let size: Option<u64> = tx
                    .query_row(
                        &format!(
                            "SELECT OCTET_LENGTH(config_json) + OCTET_LENGTH(checkpoints_json){} \
                             FROM run_contexts WHERE run_id = ?1",
                            layout.header_extra_bytes()
                        ),
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?;
                let deleted = tx.execute("DELETE FROM run_contexts WHERE run_id = ?1", [key])?;
                (
                    Work {
                        rows: deleted as u64,
                        bytes: size.unwrap_or(0),
                        ..Work::default()
                    },
                    true,
                )
            }
        }
        "thread_start_states" => match take_untracked(&tx, key, true)? {
            Untracked::Taken(work) => (work, false),
            Untracked::Oversized(_) | Untracked::None => {
                let size: Option<u64> = tx
                    .query_row(
                        "SELECT COALESCE(LENGTH(diff), 0) FROM thread_start_states \
                         WHERE thread_id = ?1",
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?;
                let deleted = tx.execute(
                    "DELETE FROM thread_start_states WHERE thread_id = ?1",
                    [key],
                )?;
                (
                    Work {
                        rows: deleted as u64,
                        bytes: size.unwrap_or(0),
                        ..Work::default()
                    },
                    true,
                )
            }
        },
        other => {
            return Err(StorageError::Serialization(format!(
                "unknown oversized table {other}"
            )));
        }
    };
    if finished {
        tx.execute(
            "DELETE FROM reclaim_oversized WHERE family_key = ?1 AND tbl = ?2 AND key = ?3",
            params![family, table, key],
        )?;
    }
    tx.execute(
        "UPDATE reclaim_jobs SET reclaimed_rows = reclaimed_rows + ?2, \
         reclaimed_stored_bytes = reclaimed_stored_bytes + ?3, updated_at_ns = ?4 \
         WHERE family_key = ?1",
        params![
            family,
            i64::try_from(work.rows).unwrap_or(i64::MAX),
            i64::try_from(work.bytes).unwrap_or(i64::MAX),
            now_ns()
        ],
    )?;
    // The job's bounded walk is over and its last oversized row is gone.
    tx.execute(
        "UPDATE reclaim_jobs SET state = 'db_done', waiting_exclusive = 0 \
         WHERE family_key = ?1 AND state = 'db_pending' AND waiting_exclusive = 1 \
         AND NOT EXISTS (SELECT 1 FROM reclaim_oversized WHERE family_key = ?1)",
        [family],
    )?;
    tx.commit()?;
    Ok(work)
}

/// Records what happened to a family's files.
pub(crate) fn mark_files(
    conn: &Connection,
    family_key: &str,
    outcome: FilesOutcome,
) -> Result<bool, StorageError> {
    let (target, from) = match outcome {
        FilesOutcome::Pending => (
            "files_pending",
            "('db_done', 'files_failed', 'files_pending')",
        ),
        FilesOutcome::Done => ("done", "('db_done', 'files_pending', 'files_failed')"),
        FilesOutcome::Failed => ("files_failed", "('db_done', 'files_pending')"),
    };
    let changed = conn.execute(
        &format!(
            "UPDATE reclaim_jobs SET state = ?2, updated_at_ns = ?3 \
             WHERE family_key = ?1 AND state IN {from}"
        ),
        params![family_key, target, now_ns()],
    )?;
    Ok(changed > 0)
}

pub(crate) fn summary(conn: &Connection) -> Result<ReclaimSummary, StorageError> {
    let mut summary = ReclaimSummary::default();
    let mut statement = conn.prepare(
        "SELECT state, COUNT(*), COALESCE(SUM(estimate_bytes - reclaimed_stored_bytes), 0) \
         FROM reclaim_jobs GROUP BY state",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (state, jobs, remaining) = row?;
        match state.as_str() {
            "db_pending" => {
                summary.jobs_db_pending = jobs;
                summary.estimate_bytes_remaining = u64::try_from(remaining).unwrap_or(0);
            }
            "db_done" => summary.jobs_db_done = jobs,
            "files_pending" => summary.jobs_files_pending = jobs,
            "files_failed" => summary.jobs_files_failed = jobs,
            "done" => summary.jobs_done = jobs,
            _ => {}
        }
    }
    let (rows, bytes): (u64, u64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(stored_bytes), 0) FROM reclaim_oversized",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    summary.oversized_rows = rows;
    summary.oversized_bytes = bytes;
    Ok(summary)
}

/// Jobs in `state` in family-key order after `after`, at most `limit`, with the members the
/// tombstone tables hold for each family. A family is small by construction (the
/// whole tombstone set is read into memory at startup).
pub(crate) fn jobs_in_state(
    conn: &Connection,
    state: ReclaimJobState,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<ReclaimJobMembers>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT family_key FROM reclaim_jobs INDEXED BY idx_reclaim_jobs_state \
         WHERE state = ?1 AND family_key > ?3 ORDER BY family_key LIMIT ?2",
    )?;
    let families = statement
        .query_map(
            params![state.as_str(), limit as i64, after.unwrap_or("")],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let column = |sql: &str, family: &str| -> Result<Vec<String>, StorageError> {
        let mut statement = conn.prepare_cached(sql)?;
        let rows = statement.query_map([family], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    };
    families
        .into_iter()
        .map(|family_key| {
            Ok(ReclaimJobMembers {
                threads: column(
                    "SELECT thread_id FROM thread_tombstones WHERE family_key = ?1 \
                     ORDER BY thread_id",
                    &family_key,
                )?,
                runs: column(
                    "SELECT run_id FROM run_tombstones WHERE family_key = ?1 ORDER BY run_id",
                    &family_key,
                )?,
                family_key,
            })
        })
        .collect()
}

#[cfg(test)]
mod lane_delivery_tests {
    use super::*;
    #[test]
    fn deleting_a_destination_reclaims_cross_thread_delivery_rows_in_bounded_steps() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        crate::migrations::apply_migrations(&conn).unwrap();
        crate::repo::lanes::create(
            &conn,
            &crate::lanes::NewLane {
                lane_id: "lane".into(),
                thread_id: "source".into(),
                origin_thread_id: "destination".into(),
                origin_run_id: "root".into(),
                creation_key: "open".into(),
                brief: crate::lanes::LaneBrief {
                    objective: "task".into(),
                    inputs: vec![],
                    deliverables: vec![],
                    acceptance_criteria: vec!["test".into()],
                    constraints: vec![],
                    workspace: Default::default(),
                    report_policy: crate::lanes::LanePolicy::Attached,
                },
                start_seed: "seed".into(),
                project_id: None,
                project_root: None,
                title: None,
                at_ns: 1,
            },
        )
        .unwrap();
        conn.execute_batch("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES ('q','r','r','root','{}');
            INSERT INTO lane_question_routes(question_id,lane_id,requester_run_id,requester_root_run_id,requester_thread_id,responder_thread_id,management_revision,routing_revision,payload) VALUES ('q','lane','r','r','source','destination',1,1,'{}');
            INSERT INTO lane_notifications(notification_id,lane_id,management_revision,event_kind,semantic_key,source_thread_id,destination_thread_id,payload) VALUES ('n','lane',1,'ready','sha','source','destination','{}');
            INSERT INTO lane_delivery_destinations(thread_id) VALUES ('destination');").unwrap();
        let mut removed = 0;
        loop {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let work = take_lane_rows(&tx, "destination").unwrap();
            assert!(work.rows <= RECLAIM_ROWS as u64);
            removed += work.rows;
            tx.commit().unwrap();
            if work.rows == 0 {
                break;
            }
        }
        assert_eq!(removed, 3);
        for table in [
            "lane_question_routes",
            "lane_notifications",
            "lane_delivery_destinations",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        }
        assert!(crate::repo::lanes::get(&conn, "lane").unwrap().is_some());
        conn.execute_batch(
            "INSERT INTO lane_stall_episodes(episode_id,lane_id,management_revision,progress_epoch,payload)
             VALUES ('episode','lane',1,0,'{}');
             INSERT INTO lane_verifications(verification_id,lane_id,reviewer_run_id,payload)
             VALUES ('verification','lane','reviewer','{}');
             INSERT INTO lane_closes(close_id,lane_id,verification_id,payload)
             VALUES ('close','lane','verification','{}');
             INSERT INTO lane_forward_receipts(caller_root,operation_id,lane_id,question_id,request_json,result_json)
             VALUES ('root','op','lane','q','{}','{}');
             INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns)
             VALUES ('source','family',10);",
        ).unwrap();
        for table in ["lane_stall_episodes", "lane_verifications", "lane_closes"] {
            assert!(
                conn.execute(&format!("UPDATE {table} SET payload=payload"), [])
                    .is_err()
            );
        }
        let mut reclaimed_bytes = 0;
        loop {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let work = take_lane_rows(&tx, "source").unwrap();
            assert!(work.rows <= RECLAIM_ROWS as u64);
            reclaimed_bytes += work.bytes;
            tx.commit().unwrap();
            if work.rows == 0 {
                break;
            }
        }
        assert!(reclaimed_bytes >= 10);
        for table in [
            "lane_stall_episodes",
            "lane_verifications",
            "lane_closes",
            "lane_forward_receipts",
            "lanes",
        ] {
            assert_eq!(
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                    .get::<_, u64>(0))
                    .unwrap(),
                0
            );
        }
    }
}
