use rusqlite::{Connection, OptionalExtension, params};

use crate::{RunContextRecord, StorageError};

pub fn upsert(conn: &Connection, record: &RunContextRecord) -> Result<(), StorageError> {
    let redactor = secret_guard::SecretRedactor::from_env();
    let mut messages: serde_json::Value = serde_json::from_str(&record.messages_json)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let mut checkpoints: serde_json::Value = serde_json::from_str(&record.checkpoints_json)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let mut count = 0;
    if let Some(messages) = messages.as_array_mut() {
        for message in messages {
            count += redact_message(&redactor, message);
        }
    } else {
        count += redactor.redact_json(&mut messages);
    }
    if let Some(checkpoints) = checkpoints.as_array_mut() {
        for checkpoint in checkpoints {
            if let Some(summary) = checkpoint.get_mut("summary") {
                count += redact_message(&redactor, summary);
            } else {
                count += redactor.redact_json(checkpoint);
            }
        }
    } else {
        count += redactor.redact_json(&mut checkpoints);
    }
    let messages_json = serde_json::to_string(&messages)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let checkpoints_json = serde_json::to_string(&checkpoints)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
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
            messages_json,
            checkpoints_json,
            record.terminal_phase,
            record.restorable,
            record.updated_at_ns
        ],
    )?;
    if count > 0 {
        tracing::info!(run_id = %record.run_id, redacted_spans = count, "run context saved with credential redactions");
    }
    Ok(())
}

/// Only payloads are redacted: roles, tool IDs/names, and checkpoint ranges stay intact.
fn redact_message(
    redactor: &secret_guard::SecretRedactor,
    message: &mut serde_json::Value,
) -> usize {
    let Some(blocks) = message
        .get_mut("content")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return redactor.redact_json(message);
    };
    let mut count = 0;
    for block in blocks {
        let field = match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text" | "reasoning") => "text",
            Some("tool_use") => "input",
            Some("tool_result") => "content",
            _ => continue,
        };
        if let Some(payload) = block.get_mut(field) {
            count += redactor.redact_json(payload);
        }
    }
    count
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

pub fn invalidate(conn: &Connection, run_id: &str) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE run_contexts SET restorable = 0, \
         config_json = json_set(config_json, '$.restorable', json('false'), \
         '$.non_restorable_reason', 'persist_failed') WHERE run_id = ?1",
        [run_id],
    )?;
    Ok(())
}
