use rusqlite::{Connection, OptionalExtension, params};

use crate::{RunContextRecord, StorageError};

/// Legacy entry point without runtime provenance: the snapshot is saved as an
/// atomic bundle whose sidecar is `unknown(legacy_unproven)` (ADR 0041 R1).
pub fn upsert(
    conn: &Connection,
    record: &RunContextRecord,
) -> Result<crate::RunContextReceipt, StorageError> {
    crate::repo::thread_index::stage_legacy_snapshot(conn, record)
        .map(|(_, receipt)| receipt)
        .map_err(crate::repo::thread_index::into_storage_error)
}

/// A snapshot after credential redaction, serialized exactly as it is stored.
pub(crate) struct RedactedSnapshot {
    pub messages: serde_json::Value,
    pub messages_json: String,
    pub checkpoints_json: String,
    pub checkpoint_count: usize,
}

impl RedactedSnapshot {
    pub(crate) fn messages_end(&self) -> usize {
        self.messages.as_array().map_or(0, Vec::len)
    }
}

/// Unparseable JSON is reported as `Ok(None)` so callers can map it to their API.
pub(crate) fn redact(record: &RunContextRecord) -> Result<Option<RedactedSnapshot>, StorageError> {
    let redactor = secret_guard::SecretRedactor::from_env();
    let (Ok(mut messages), Ok(mut checkpoints)) = (
        serde_json::from_str::<serde_json::Value>(&record.messages_json),
        serde_json::from_str::<serde_json::Value>(&record.checkpoints_json),
    ) else {
        return Ok(None);
    };
    let mut count = 0;
    if let Some(messages) = messages.as_array_mut() {
        for message in messages {
            count += redact_message(&redactor, message)?;
        }
    } else {
        count += redactor.redact_json(&mut messages);
    }
    if let Some(checkpoints) = checkpoints.as_array_mut() {
        for checkpoint in checkpoints {
            if let Some(summary) = checkpoint.get_mut("summary") {
                count += redact_message(&redactor, summary)?;
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
    if count > 0 {
        tracing::info!(run_id = %record.run_id, redacted_spans = count, "run context saved with credential redactions");
    }
    Ok(Some(RedactedSnapshot {
        checkpoint_count: checkpoints.as_array().map_or(0, Vec::len),
        messages,
        messages_json,
        checkpoints_json,
    }))
}

/// Writes only the snapshot row; the bundle API owns binding, sidecar and state.
pub(crate) fn write(
    conn: &Connection,
    record: &RunContextRecord,
    redacted: &RedactedSnapshot,
    snapshot_revision: u64,
) -> Result<(), StorageError> {
    let snapshot_revision = i64::try_from(snapshot_revision)
        .map_err(|_| StorageError::OutOfRange("snapshot revision"))?;
    conn.execute(
        "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json, \
         checkpoints_json, terminal_phase, restorable, updated_at_ns, snapshot_revision) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT(run_id) DO UPDATE SET role = excluded.role, name = excluded.name, \
         parent_run_id = excluded.parent_run_id, config_json = excluded.config_json, \
         messages_json = excluded.messages_json, checkpoints_json = excluded.checkpoints_json, \
         terminal_phase = excluded.terminal_phase, restorable = excluded.restorable, \
         updated_at_ns = excluded.updated_at_ns, snapshot_revision = excluded.snapshot_revision",
        params![
            record.run_id,
            record.role,
            record.name,
            record.parent_run_id,
            record.config_json,
            redacted.messages_json,
            redacted.checkpoints_json,
            record.terminal_phase,
            record.restorable,
            record.updated_at_ns,
            snapshot_revision
        ],
    )?;
    Ok(())
}

/// Only payloads are redacted: roles, tool IDs/names, and checkpoint ranges stay intact.
fn redact_message(
    redactor: &secret_guard::SecretRedactor,
    message: &mut serde_json::Value,
) -> Result<usize, StorageError> {
    let Some(blocks) = message
        .get_mut("content")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(redactor.redact_json(message));
    };
    let mut count = 0;
    for block in blocks {
        let field = match block.get("type").and_then(serde_json::Value::as_str) {
            Some("anthropic") => {
                if let Some(payload) = block.get_mut("block") {
                    count += redact_anthropic_block(redactor, payload)?;
                }
                continue;
            }
            Some("text" | "reasoning") => "text",
            Some("tool_use") => "input",
            Some("tool_result") => {
                if let Some(parts) = block
                    .get_mut("content")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for part in parts {
                        if part.get("type").and_then(serde_json::Value::as_str) != Some("image") {
                            count += redactor.redact_json(part);
                        }
                    }
                }
                continue;
            }
            _ => continue,
        };
        if let Some(payload) = block.get_mut(field) {
            count += redactor.redact_json(payload);
        }
    }
    Ok(count)
}

fn redact_anthropic_block(
    redactor: &secret_guard::SecretRedactor,
    block: &mut serde_json::Value,
) -> Result<usize, StorageError> {
    let protected_fields: &[&str] = match block.get("type").and_then(serde_json::Value::as_str) {
        Some("thinking") => &["signature"],
        Some("redacted_thinking") => &["data"],
        Some("compaction") => &["signature", "encrypted_content"],
        _ => &[],
    };
    let protected: Vec<_> = protected_fields
        .iter()
        .filter_map(|&field| {
            let object = block.as_object_mut()?;
            object.get(field)?.as_str()?;
            object.remove(field).map(|value| (field, value))
        })
        .collect();
    let count = redactor.redact_json(block);
    let signed = protected.iter().any(|(field, _)| *field == "signature");
    for (field, value) in protected {
        block.as_object_mut().unwrap().insert(field.into(), value);
    }
    if signed && count > 0 {
        return Err(StorageError::Serialization(
            "signed Anthropic content contains a secret; redaction would invalidate its signature"
                .into(),
        ));
    }
    Ok(count)
}

/// Columns decoded by [`from_row`], in order.
pub(crate) const COLUMNS: &str = "run_id, role, name, parent_run_id, config_json, messages_json, \
                                  checkpoints_json, terminal_phase, restorable, updated_at_ns";

pub fn get(conn: &Connection, run_id: &str) -> Result<Option<RunContextRecord>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM run_contexts WHERE run_id = ?1"),
            [run_id],
            from_row,
        )
        .optional()?)
}

pub(crate) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunContextRecord> {
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
}

pub fn compare_and_swap_restore_state(
    conn: &Connection,
    run_id: &str,
    expected_config: &str,
    retry_config: &str,
    retry_restorable: bool,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE run_contexts SET config_json = ?3, restorable = ?4 \
         WHERE run_id = ?1 AND config_json = ?2 AND restorable = 0",
        params![run_id, expected_config, retry_config, retry_restorable],
    )? == 1)
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

/// `run_ids` and every run delegated from them, through the parent links of
/// saved contexts and of the usage ledger.
pub fn with_descendants(
    conn: &Connection,
    run_ids: &[String],
) -> Result<Vec<String>, StorageError> {
    let ids = serde_json::to_string(run_ids)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let mut statement = conn.prepare(
        "WITH RECURSIVE tree(run_id) AS (\
         SELECT value FROM json_each(?1) \
         UNION SELECT c.run_id FROM run_contexts c JOIN tree t ON c.parent_run_id = t.run_id \
         UNION SELECT u.run_id FROM usage_requests u JOIN tree t ON u.parent_run_id = t.run_id \
         WHERE u.run_id IS NOT NULL) \
         SELECT run_id FROM tree ORDER BY run_id",
    )?;
    let rows = statement.query_map(params![ids], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
