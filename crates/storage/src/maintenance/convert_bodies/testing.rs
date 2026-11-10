//! Test support: turns a database in the current format back into the version 39 shape,
//! so that a test can start from what the running build wrote and run the conversion on
//! it. Not a product path: the result is what an old build would have left, rebuilt from
//! the rows the current reader returns.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, params};

use crate::StorageError;
use crate::codec;
use crate::migrations::MIGRATIONS;
use crate::repo::run_messages::get_with_revision;

use super::super::invalid;

/// Rewrites `path` as a version 39 database: every run's conversation goes back into
/// `run_contexts.messages_json` (zstd when it is large, as the old writer stored it), the
/// version 40 tables are dropped, the blobs are removed, and `run_contexts` is created
/// again from the SQL that replaying migrations 1 to 39 produces. Nothing else changes.
///
/// # Errors
/// Fails when the database cannot be read or rewritten.
pub fn downgrade_to_v39(path: &Path) -> Result<(), StorageError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let ids: Vec<String> = conn
        .prepare("SELECT run_id FROM run_contexts ORDER BY run_id")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in &ids {
        let (record, revision) = get_with_revision(&conn, id)?
            .ok_or_else(|| invalid(format!("run {id} disappeared while downgrading")))?;
        rows.push((record, revision));
    }
    let fresh = Connection::open_in_memory()?;
    for migration in &MIGRATIONS[..39] {
        fresh.execute_batch(migration)?;
    }
    let definitions_of = |tables: &str| -> Result<Vec<String>, StorageError> {
        Ok(fresh
            .prepare(&format!(
                "SELECT sql FROM sqlite_master WHERE tbl_name IN ({tables}) AND sql IS NOT NULL \
                 ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, name"
            ))?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?)
    };
    let definitions = definitions_of("'run_contexts'")?;
    // The lesson tables v42 rebuilt (favorite flag, two more ledger kinds), as v39 had them.
    let lesson_definitions = definitions_of("'memory_ledger', 'memory_entries'")?;
    let accounting: Vec<String> = fresh
        .prepare(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' \
             AND name IN ('events_accounting_insert', 'events_accounting_delete', \
                          'events_accounting_update') ORDER BY name",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    // Tokenized events are written back as plain ones while their blobs still exist.
    resolve_tokenized_events(&conn)?;
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    conn.execute_batch(concat!(
        concat!(
            include_str!("../../../undo/undo_v59.sql"),
            include_str!("../../../undo/undo_v58.sql"),
            include_str!("../../../undo/undo_v57.sql"),
            include_str!("../../../undo/undo_v56.sql"),
            include_str!("../../../undo/undo_v55.sql"),
            include_str!("../../../undo/undo_v54.sql"),
            include_str!("../../../undo/undo_v53.sql"),
            include_str!("../../../undo/undo_v52.sql"),
            include_str!("../../../undo/undo_v51.sql"),
            include_str!("../../../undo/undo_v50.sql"),
            include_str!("../../../undo/undo_v49.sql"),
            include_str!("../../../undo/undo_v48.sql"),
            include_str!("../../../undo/undo_v47.sql")
        ),
        include_str!("../../../undo/undo_v46.sql")
    ))?;
    conn.execute_batch(include_str!("../../../undo/undo_v43.sql"))?;
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         DROP TRIGGER IF EXISTS content_blob_refs_event_queue_gc;
         DROP TABLE IF EXISTS content_blob_refs_event;
         DROP TRIGGER events_accounting_insert;
         DROP TRIGGER events_accounting_delete;
         DROP TRIGGER events_accounting_update;
         ALTER TABLE events DROP COLUMN body_format;
         ALTER TABLE events DROP COLUMN blob_bytes;
         DROP TRIGGER IF EXISTS run_messages_drop_refs;
         DROP TABLE content_blob_refs_message;
         DROP TABLE content_blob_refs_header;
         DROP TABLE run_messages;
         DELETE FROM content_blob_gc_queue;
         DELETE FROM content_blobs;
         DROP TABLE lane_list_snapshot_rows;
         DROP TABLE lane_list_snapshots;
         DROP TABLE lane_delivery_keys;
         DROP TABLE lane_reports;
         DROP TABLE lanes;
         DROP TABLE lane_state;
         DROP TRIGGER threads_lane_lineage_sticky;
         DROP TABLE run_contexts;",
    )?;
    for definition in &definitions {
        conn.execute_batch(definition)?;
    }
    for definition in &accounting {
        conn.execute_batch(definition)?;
    }
    conn.execute_batch(
        "DELETE FROM event_session_bytes;
         INSERT INTO event_session_bytes SELECT session_id, SUM(OCTET_LENGTH(payload)) FROM events \
             WHERE session_id IS NOT NULL GROUP BY session_id;
         DELETE FROM event_day_bytes;
         INSERT INTO event_day_bytes SELECT wall_clock_ns / 86400000000000 \
             - (wall_clock_ns % 86400000000000 < 0), SUM(OCTET_LENGTH(payload)) FROM events GROUP BY 1;
         UPDATE sessions SET total_event_bytes = COALESCE( \
             (SELECT payload_bytes FROM event_session_bytes WHERE session_id = sessions.id), 0);",
    )?;
    take_lessons_back(&conn, &lesson_definitions)?;
    for (record, revision) in &rows {
        conn.execute(
            "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, \
             messages_json, checkpoints_json, terminal_phase, restorable, updated_at_ns, \
             host_inputs_json, snapshot_revision, messages_len, checkpoints_len) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.run_id,
                record.role,
                record.name,
                record.parent_run_id,
                record.config_json,
                codec::encode(&record.messages_json),
                codec::encode(&record.checkpoints_json),
                record.terminal_phase,
                record.restorable,
                record.updated_at_ns,
                record.host_inputs_json,
                i64::try_from(*revision).map_err(|_| StorageError::OutOfRange("revision"))?,
                i64::try_from(record.messages_json.len())
                    .map_err(|_| StorageError::OutOfRange("messages length"))?,
                i64::try_from(record.checkpoints_json.len())
                    .map_err(|_| StorageError::OutOfRange("checkpoints length"))?,
            ],
        )?;
    }
    conn.pragma_update(None, "user_version", 39)?;
    conn.execute_batch("COMMIT")?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(())
}

/// Writes every tokenized event back as a plain one (its bodies resolved), the form a
/// database before the event body format holds.
fn resolve_tokenized_events(conn: &Connection) -> Result<(), StorageError> {
    let ids: Vec<(i64, String, Option<String>)> = conn
        .prepare("SELECT id, kind, subkind FROM events WHERE body_format = 1 ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    for (id, kind, subkind) in ids {
        let plain = conn.query_row("SELECT payload FROM events WHERE id = ?1", [id], |row| {
            crate::repo::event::resolved_payload_json(
                conn,
                row.get_ref(0)?,
                crate::repo::event::StoredFormat::Tokenized,
                "downgrade",
            )
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        let subkind = subkind.unwrap_or_default();
        let encoded = crate::repo::event::encode_payload(&kind, &subkind, &plain);
        conn.execute(
            "UPDATE events SET payload = ?2, body_format = 0 WHERE id = ?1",
            params![id, encoded],
        )?;
    }
    Ok(())
}

/// Rewrites `path` as a version 43 database: what a build before the event body format
/// wrote. Tokenized events are written back as plain ones, and the event columns, the
/// reference table and the accounting triggers of the format are removed; the runs and
/// the provenance rows stay as they are.
///
/// # Errors
/// Fails when the database cannot be read or rewritten.
pub fn downgrade_to_v43(path: &Path) -> Result<(), StorageError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(concat!(
        concat!(
            include_str!("../../../undo/undo_v59.sql"),
            include_str!("../../../undo/undo_v58.sql"),
            include_str!("../../../undo/undo_v57.sql"),
            include_str!("../../../undo/undo_v56.sql"),
            include_str!("../../../undo/undo_v55.sql"),
            include_str!("../../../undo/undo_v54.sql"),
            include_str!("../../../undo/undo_v53.sql"),
            include_str!("../../../undo/undo_v52.sql"),
            include_str!("../../../undo/undo_v51.sql"),
            include_str!("../../../undo/undo_v50.sql"),
            include_str!("../../../undo/undo_v49.sql"),
            include_str!("../../../undo/undo_v48.sql"),
            include_str!("../../../undo/undo_v47.sql")
        ),
        include_str!("../../../undo/undo_v46.sql")
    ))?;
    let fresh = Connection::open_in_memory()?;
    for migration in &MIGRATIONS[..43] {
        fresh.execute_batch(migration)?;
    }
    let accounting: Vec<String> = fresh
        .prepare(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' \
             AND name IN ('events_accounting_insert', 'events_accounting_delete', \
                          'events_accounting_update') ORDER BY name",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    resolve_tokenized_events(&conn)?;
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         DROP TRIGGER IF EXISTS content_blob_refs_event_queue_gc;
         DROP TABLE IF EXISTS content_blob_refs_event;
         DROP TRIGGER events_accounting_insert;
         DROP TRIGGER events_accounting_delete;
         DROP TRIGGER events_accounting_update;
         ALTER TABLE events DROP COLUMN body_format;
         ALTER TABLE events DROP COLUMN blob_bytes;",
    )?;
    for definition in &accounting {
        conn.execute_batch(definition)?;
    }
    conn.execute_batch(
        "DELETE FROM event_session_bytes;
         INSERT INTO event_session_bytes SELECT session_id, SUM(OCTET_LENGTH(payload)) FROM events \
             WHERE session_id IS NOT NULL GROUP BY session_id;
         DELETE FROM event_day_bytes;
         INSERT INTO event_day_bytes SELECT wall_clock_ns / 86400000000000 \
             - (wall_clock_ns % 86400000000000 < 0), SUM(OCTET_LENGTH(payload)) FROM events GROUP BY 1;
         UPDATE sessions SET total_event_bytes = COALESCE( \
             (SELECT payload_bytes FROM event_session_bytes WHERE session_id = sessions.id), 0);",
    )?;
    conn.execute_batch(include_str!("../../../undo/undo_v45.sql"))?;
    conn.pragma_update(None, "user_version", 43)?;
    conn.execute_batch("COMMIT")?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(())
}

/// Rewrites `path` as a version 40 database: a version 43 one (see [`downgrade_to_v43`])
/// whose memory favorite flag and lane ledger are gone and whose provenance maps are one
/// column again; the runs stay as they are.
///
/// # Errors
/// Fails when the database cannot be read or rewritten.
pub fn downgrade_to_v40(path: &Path) -> Result<(), StorageError> {
    downgrade_provenance(path, 40)
}

/// Rewrites `path` as a version 41 database: a version 43 one (see [`downgrade_to_v43`])
/// whose memory favorite flag is gone and whose provenance maps are one column again (the
/// lane ledger stays).
///
/// # Errors
/// Fails when the database cannot be read or rewritten.
pub fn downgrade_to_v41(path: &Path) -> Result<(), StorageError> {
    downgrade_provenance(path, 41)
}

/// Rewrites `path` as a version 42 database: a version 43 one (see [`downgrade_to_v43`])
/// whose provenance maps are one column again (the lane ledger and the favorite flag stay).
///
/// # Errors
/// Fails when the database cannot be read or rewritten.
pub fn downgrade_to_v42(path: &Path) -> Result<(), StorageError> {
    downgrade_provenance(path, 42)
}

fn downgrade_provenance(path: &Path, version: u32) -> Result<(), StorageError> {
    downgrade_to_v43(path)?;
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    conn.execute_batch(include_str!("../../../undo/undo_v43.sql"))?;
    if version <= 41 {
        take_lessons_back_to_v41(&conn)?;
    }
    if version <= 40 {
        conn.execute_batch(include_str!("../../../undo/undo_v41.sql"))?;
    }
    conn.pragma_update(None, "user_version", version)?;
    conn.execute_batch("COMMIT")?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(())
}

/// Takes the lesson tables of a database in the current format back to their version 41
/// shape (what migration v42 found), for a test that rebuilds an older database by hand.
///
/// # Errors
/// Fails when the tables cannot be rebuilt.
pub(crate) fn take_lessons_back_to_v41(conn: &Connection) -> Result<(), StorageError> {
    let fresh = Connection::open_in_memory()?;
    for migration in &MIGRATIONS[..39] {
        fresh.execute_batch(migration)?;
    }
    let definitions: Vec<String> = fresh
        .prepare(
            "SELECT sql FROM sqlite_master WHERE tbl_name IN ('memory_ledger', 'memory_entries') \
             AND sql IS NOT NULL ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, name",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    take_lessons_back(conn, &definitions)
}

/// Rebuilds the lesson tables as migration v42 found them, keeping every lesson in its
/// position: favorite marks and the ledger rows that record them are dropped.
fn take_lessons_back(conn: &Connection, definitions: &[String]) -> Result<(), StorageError> {
    conn.execute_batch(
        "CREATE TEMP TABLE ledger_keep AS
             SELECT seq, entry_id, project, task_id, content, evidence, status, kind, scope
             FROM memory_ledger WHERE kind IN ('lesson', 'finding', 'eval_trace');
         CREATE TEMP TABLE entries_keep AS
             SELECT rowid AS entry_rowid, id, project, task_id, content, evidence, status,
                    ledger_seq, scope
             FROM memory_entries;
         DROP TABLE memory_entries;
         DROP TABLE memory_ledger;",
    )?;
    for definition in definitions {
        conn.execute_batch(definition)?;
    }
    conn.execute_batch(
        "INSERT INTO memory_ledger(seq, entry_id, project, task_id, content, evidence, status, kind, scope)
             SELECT seq, entry_id, project, task_id, content, evidence, status, kind, scope
             FROM ledger_keep ORDER BY seq;
         INSERT INTO memory_entries(rowid, id, project, task_id, content, evidence, status, ledger_seq, scope)
             SELECT entry_rowid, id, project, task_id, content, evidence, status, ledger_seq, scope
             FROM entries_keep ORDER BY entry_rowid;
         INSERT INTO memory_fts(memory_fts) VALUES('rebuild');
         DROP TABLE ledger_keep;
         DROP TABLE entries_keep;",
    )?;
    Ok(())
}
