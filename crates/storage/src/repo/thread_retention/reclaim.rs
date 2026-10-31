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
    RECLAIM_INDEX_TEXT_BYTES, RECLAIM_ROW_BUDGET, RECLAIM_ROWS, RECLAIM_SCAN_ROWS, ReclaimBudget,
    ReclaimProgress, ReclaimSummary,
};

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
    rows: u64,
    bytes: u64,
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
            Phase::EventsRun | Phase::RunTables | Phase::Index | Phase::RunContexts => {
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
            Phase::Index => Phase::RunContexts,
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
        Phase::Index => Phase::RunContexts,
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
fn do_phase(tx: &Transaction<'_>, family: &str, cursor: &mut Cursor) -> Result<Work, StorageError> {
    match cursor.phase {
        Phase::EventsRun => events_run(tx, family, cursor),
        Phase::EventsThread => events_thread(tx, family, cursor),
        Phase::RunTables => run_tables(tx, family, cursor),
        Phase::Index => index(tx, family, cursor),
        Phase::RunContexts => run_contexts(tx, family, cursor),
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
    let mut per_session: Vec<(Option<String>, u64)> = Vec::new();
    let mut bytes = 0_u64;
    let mut oversized: Option<(i64, u64)> = None;
    {
        let mut statement = tx.prepare_cached(
            "SELECT id, OCTET_LENGTH(payload), session_id FROM events \
             WHERE run_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
        )?;
        let limit = i64::try_from(RECLAIM_EVENT_ROWS).unwrap_or(i64::MAX);
        let mut rows = statement.query(params![run, cursor.after, limit])?;
        while let Some(row) = rows.next()? {
            let size = row.get::<_, u64>(1)?;
            if ids.is_empty() && size > RECLAIM_EVENT_BYTES {
                oversized = Some((row.get::<_, i64>(0)?, size));
                break;
            }
            if !ids.is_empty() && bytes.saturating_add(size) > RECLAIM_EVENT_BYTES {
                break;
            }
            ids.push(row.get::<_, i64>(0)?);
            bytes = bytes.saturating_add(size);
            let session: Option<String> = row.get(2)?;
            match per_session.iter_mut().find(|(known, _)| *known == session) {
                Some((_, total)) => *total = total.saturating_add(size),
                None => per_session.push((session, size)),
            }
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
    for (session, size) in per_session {
        crate::maintenance::adjust_session_bytes(tx, session.as_deref(), size, 0)?;
    }
    cursor.after = last;
    Ok(Work {
        rows: ids.len() as u64,
        bytes,
    })
}

/// Thread todo snapshots carry no run: find them by the thread they name, scanning
/// a bounded number of rows per step.
fn events_thread(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
) -> Result<Work, StorageError> {
    let mut scanned = 0_usize;
    let mut last = cursor.scan_after;
    let mut ids = Vec::new();
    let mut per_session: Vec<(Option<String>, u64)> = Vec::new();
    let mut bytes = 0_u64;
    {
        let mut statement = tx.prepare_cached(
            "SELECT id, OCTET_LENGTH(payload), session_id, \
             EXISTS(SELECT 1 FROM thread_tombstones \
                    WHERE thread_id = json_extract(payload, '$.payload.payload.snapshot.thread_id') \
                      AND family_key = ?3) FROM events \
             WHERE kind = 'Orchestrator' AND subkind = 'ThreadTodoUpdated' \
             AND run_id IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let limit = i64::try_from(RECLAIM_SCAN_ROWS).unwrap_or(i64::MAX);
        let mut rows = statement.query(params![cursor.scan_after, limit, family])?;
        while let Some(row) = rows.next()? {
            scanned += 1;
            last = row.get(0)?;
            // A point lookup per scanned row, so a family with many threads costs
            // no more than a small one.
            if row.get::<_, bool>(3)? {
                let size = row.get::<_, u64>(1)?;
                ids.push(last);
                bytes = bytes.saturating_add(size);
                let session: Option<String> = row.get(2)?;
                match per_session.iter_mut().find(|(known, _)| *known == session) {
                    Some((_, total)) => *total = total.saturating_add(size),
                    None => per_session.push((session, size)),
                }
            }
        }
    }
    if !ids.is_empty() {
        tx.execute(
            "DELETE FROM events WHERE id IN (SELECT value FROM json_each(?1))",
            [json_ids(&ids)?],
        )?;
        for (session, size) in per_session {
            crate::maintenance::adjust_session_bytes(tx, session.as_deref(), size, 0)?;
        }
    }
    cursor.scan_after = last;
    if scanned < RECLAIM_SCAN_ROWS {
        enter(tx, family, cursor, Phase::RunTables)?;
    }
    Ok(Work {
        rows: ids.len() as u64,
        bytes,
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
    })
}

fn index(tx: &Transaction<'_>, family: &str, cursor: &mut Cursor) -> Result<Work, StorageError> {
    let Some(run) = cursor.run.clone() else {
        enter(tx, family, cursor, Phase::RunContexts)?;
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
    })
}

fn run_contexts(
    tx: &Transaction<'_>,
    family: &str,
    cursor: &mut Cursor,
) -> Result<Work, StorageError> {
    let Some(run) = cursor.run.clone() else {
        enter(tx, family, cursor, Phase::Tasks)?;
        return Ok(Work::default());
    };
    let stored: Option<u64> = tx
        .query_row(
            "SELECT OCTET_LENGTH(messages_json) + OCTET_LENGTH(checkpoints_json) \
             FROM run_contexts WHERE run_id = ?1",
            [&run],
            |row| row.get(0),
        )
        .optional()?;
    let mut work = Work::default();
    match stored {
        None => {}
        Some(bytes) if bytes > RECLAIM_ROW_BUDGET => {
            register_oversized(tx, family, "run_contexts", &run, bytes)?;
        }
        Some(bytes) => {
            tx.execute("DELETE FROM run_contexts WHERE run_id = ?1", [&run])?;
            work = Work { rows: 1, bytes };
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
    }))
}

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
    if cursor.table_idx == 0 {
        match take_untracked(tx, &thread, false)? {
            Untracked::Taken(taken) => work = taken,
            // A captured file above the step's budget cannot be deleted by a bounded
            // step. The state row owns the untracked rows (ON DELETE CASCADE), so the
            // capture is registered once, by the size of that file, and deleted in
            // batches by the exclusive phase.
            Untracked::Oversized(size) => {
                register_oversized(tx, family, "thread_start_states", &thread, size)?;
                match next_thread(tx, family, &thread)? {
                    Some(next) => {
                        cursor.thread = Some(next);
                        cursor.table_idx = 0;
                    }
                    None => enter(tx, family, cursor, Phase::Finished)?,
                }
                return Ok(Work::default());
            }
            Untracked::None => cursor.table_idx = 1,
        }
    } else {
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
                work = Work { rows: 1, bytes };
            }
        }
        match next_thread(tx, family, &thread)? {
            Some(next) => {
                cursor.thread = Some(next);
                cursor.table_idx = 0;
            }
            None => enter(tx, family, cursor, Phase::Finished)?,
        }
    }
    Ok(work)
}

/// One transaction of one job. Returns what it deleted.
fn step_job(conn: &Connection, family: &str) -> Result<(Work, bool), StorageError> {
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
        let step = do_phase(&tx, family, &mut cursor)?;
        work.rows += step.rows;
        work.bytes += step.bytes;
        if scanning || step.rows > 0 || step.bytes > 0 {
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
         reclaimed_rows = reclaimed_rows + ?5, reclaimed_stored_bytes = reclaimed_stored_bytes + ?6 \
         WHERE family_key = ?1",
        params![
            family,
            new_state,
            serde_json::to_string(&cursor)
                .map_err(|error| StorageError::Serialization(error.to_string()))?,
            now_ns(),
            i64::try_from(work.rows).unwrap_or(i64::MAX),
            i64::try_from(work.bytes).unwrap_or(i64::MAX),
        ],
    )?;
    tx.commit()?;
    Ok((work, finished))
}

fn count(conn: &Connection, sql: &str) -> Result<u64, StorageError> {
    Ok(conn.query_row(sql, [], |row| row.get::<_, u64>(0))?)
}

/// Runs up to `budget.max_transactions` reclaim transactions over the pending
/// jobs in family order. Reads only when nothing is pending.
pub(crate) fn reclaim_step(
    conn: &Connection,
    budget: ReclaimBudget,
) -> Result<ReclaimProgress, StorageError> {
    let mut progress = ReclaimProgress::default();
    let families: Vec<String> = {
        let mut statement = conn.prepare_cached(
            "SELECT family_key FROM reclaim_jobs WHERE state = 'db_pending' \
             AND COALESCE(json_extract(cursor_json, '$.phase'), '') <> 'finished' \
             ORDER BY family_key",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?
    };
    'jobs: for family in &families {
        loop {
            if progress.transactions >= budget.max_transactions {
                break 'jobs;
            }
            let (work, finished) = match step_job(conn, family) {
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
        "SELECT COUNT(*) FROM reclaim_jobs WHERE state = 'db_pending' \
         AND COALESCE(json_extract(cursor_json, '$.phase'), '') <> 'finished'",
    )?;
    progress.jobs_waiting_exclusive = count(
        conn,
        "SELECT COUNT(*) FROM reclaim_jobs WHERE state = 'db_pending' \
         AND COALESCE(json_extract(cursor_json, '$.phase'), '') = 'finished'",
    )?;
    progress.jobs_done = count(
        conn,
        "SELECT COUNT(*) FROM reclaim_jobs WHERE state IN ('db_done', 'files_pending', 'done', 'files_failed')",
    )?;
    progress.oversized_pending = count(conn, "SELECT COUNT(*) FROM reclaim_oversized")?;
    Ok(progress)
}

/// Deletes oversized rows smallest first, one bounded transaction each, until
/// about `budget_bytes` of stored data are gone. At least one row is deleted when
/// any is waiting. A registration that owns many rows (a run's index blocks, a
/// capture's files) is worked off in batches: it stays registered, and the next
/// call or loop pass continues it, so no transaction deletes more than one batch
/// or one oversized row. A failure leaves the registration and stops the step.
///
/// `rows` and `stored_bytes` count what was actually deleted.
pub(crate) fn reclaim_oversized_step(
    conn: &Connection,
    budget_bytes: u64,
) -> Result<OversizedProgress, StorageError> {
    let mut progress = OversizedProgress::default();
    loop {
        let next: Option<(String, String, String, u64)> = conn
            .query_row(
                "SELECT family_key, tbl, key, stored_bytes FROM reclaim_oversized \
                 ORDER BY stored_bytes, family_key, tbl, key LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((family, table, key, bytes)) = next else {
            break;
        };
        if progress.rows > 0 && progress.stored_bytes.saturating_add(bytes) > budget_bytes {
            break;
        }
        match delete_oversized(conn, &family, &table, &key) {
            Ok(work) => {
                progress.rows += work.rows;
                progress.stored_bytes = progress.stored_bytes.saturating_add(work.bytes);
            }
            Err(error) => {
                progress.last_error = Some(error.to_string());
                break;
            }
        }
    }
    progress.remaining = count(conn, "SELECT COUNT(*) FROM reclaim_oversized")?;
    Ok(progress)
}

/// One transaction of the exclusive phase for one registration. The registration
/// is dropped once nothing of it is left.
fn delete_oversized(
    conn: &Connection,
    family: &str,
    table: &str,
    key: &str,
) -> Result<Work, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let (work, finished) = match table {
        "events" => {
            let id: i64 = key.parse().map_err(|_| {
                StorageError::Serialization(format!("bad oversized event id {key}"))
            })?;
            let row: Option<(Option<String>, u64)> = tx
                .query_row(
                    "SELECT session_id, OCTET_LENGTH(payload) FROM events WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let mut work = Work::default();
            if let Some((session, size)) = row {
                tx.execute("DELETE FROM events WHERE id = ?1", [id])?;
                crate::maintenance::adjust_session_bytes(&tx, session.as_deref(), size, 0)?;
                work = Work {
                    rows: 1,
                    bytes: size,
                };
            }
            (work, true)
        }
        "conversation_blocks" => {
            let progress = purge_index_rows_with(
                &tx,
                key,
                PurgeTarget::Tombstoned,
                RECLAIM_INDEX_ROWS,
                RECLAIM_INDEX_TEXT_BYTES,
                OversizedBlock::TakeAlone,
            )?;
            (
                Work {
                    rows: progress.rows as u64,
                    bytes: progress.text_bytes,
                },
                !progress.remaining,
            )
        }
        "run_contexts" => {
            let size: Option<u64> = tx
                .query_row(
                    "SELECT OCTET_LENGTH(messages_json) + OCTET_LENGTH(checkpoints_json) \
                     FROM run_contexts WHERE run_id = ?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            let deleted = tx.execute("DELETE FROM run_contexts WHERE run_id = ?1", [key])?;
            (
                Work {
                    rows: deleted as u64,
                    bytes: size.unwrap_or(0),
                },
                true,
            )
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
        "UPDATE reclaim_jobs SET state = 'db_done' WHERE family_key = ?1 AND state = 'db_pending' \
         AND json_extract(cursor_json, '$.phase') = 'finished' \
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
