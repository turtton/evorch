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
    // The lesson tables v41 rebuilt (favorite flag, two more ledger kinds), as v39 had them.
    let lesson_definitions = definitions_of("'memory_ledger', 'memory_entries'")?;
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         DROP TRIGGER IF EXISTS run_messages_drop_refs;
         DROP TABLE content_blob_refs_message;
         DROP TABLE content_blob_refs_header;
         DROP TABLE run_messages;
         DELETE FROM content_blob_gc_queue;
         DELETE FROM content_blobs;
         DROP TABLE run_contexts;",
    )?;
    for definition in &definitions {
        conn.execute_batch(definition)?;
    }
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

/// Rebuilds the lesson tables as migration v41 found them, keeping every lesson in its
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
