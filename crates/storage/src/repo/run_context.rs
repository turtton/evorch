use rusqlite::{Connection, OptionalExtension, params};

use crate::{RunContextRecord, StorageError};

pub fn upsert(conn: &Connection, record: &RunContextRecord) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json, \
         checkpoints_json, terminal_phase, restorable, updated_at_ns) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
         ON CONFLICT(run_id) DO UPDATE SET role = excluded.role, name = excluded.name, \
         parent_run_id = excluded.parent_run_id, config_json = excluded.config_json, \
         messages_json = excluded.messages_json, checkpoints_json = excluded.checkpoints_json, \
         terminal_phase = excluded.terminal_phase, restorable = excluded.restorable, \
         updated_at_ns = excluded.updated_at_ns",
        params![
            record.run_id,
            record.role,
            record.name,
            record.parent_run_id,
            record.config_json,
            record.messages_json,
            record.checkpoints_json,
            record.terminal_phase,
            record.restorable,
            record.updated_at_ns
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, run_id: &str) -> Result<Option<RunContextRecord>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT run_id, role, name, parent_run_id, config_json, messages_json, \
         checkpoints_json, terminal_phase, restorable, updated_at_ns \
         FROM run_contexts WHERE run_id = ?1",
            [run_id],
            |row| {
                Ok(RunContextRecord {
                    run_id: row.get(0)?,
                    role: row.get(1)?,
                    name: row.get(2)?,
                    parent_run_id: row.get(3)?,
                    config_json: row.get(4)?,
                    messages_json: row.get(5)?,
                    checkpoints_json: row.get(6)?,
                    terminal_phase: row.get(7)?,
                    restorable: row.get(8)?,
                    updated_at_ns: row.get(9)?,
                })
            },
        )
        .optional()?)
}
