//! A run's conversation as rows: the header in `run_contexts` and one `run_messages` row
//! per provider message.
//!
//! # Writing
//!
//! [`write_snapshot`] runs inside the caller's write transaction (see
//! `thread_index::atomic`: `BEGIN IMMEDIATE`, or a savepoint of a transaction that
//! already wrote). It writes the header, then only the messages whose hash differs from
//! the stored one at the same ordinal, then drops the rows past the new end. Messages
//! that did not change cost a metadata read (`id`, `ordinal`, `msg_sha256`) and nothing
//! else: no skeleton, no blob, no reference row. Every long string goes through
//! `body_store::put_owner`, so a blob, the owner row naming its token and the owner's
//! reference rows are written together.
//!
//! # Reading
//!
//! Readers rebuild the exact text the writer was given: each skeleton is decoded, its
//! tokens are replaced by their bodies, and the message is serialized again with the same
//! serializer. The messages are joined as `[m0,m1,...]`, which is how the whole array was
//! serialized, so the bytes (and the snapshot hash computed from them) are those that
//! were saved. Header, messages and blobs are read in one read snapshot.
//!
//! A missing blob, a length or ordinal that does not add up, or a header in another body
//! format is an error. Nothing here substitutes an empty or placeholder text.

use std::collections::HashMap;

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::body_ref::{BodyToken, resolve_in_place, tokenize_in_place};
use crate::body_store::{
    HEADER_OWNER, MESSAGE_OWNER, decode_skeleton, encode_text, load_blob, put_owner,
    put_owner_with_stored, unresolved,
};
use crate::error::BodyUnresolvedReason;
use crate::repo::run_context::RedactedSnapshot;
use crate::{RunContextRecord, StorageError};

/// The only body format a running build reads or writes.
pub(crate) const BODY_FORMAT: i64 = 1;

/// Columns of the header read by [`HeaderRow::from_row`], in order.
pub(crate) const HEADER_COLUMNS: &str = "run_id, role, name, parent_run_id, config_json, \
    checkpoints_json, terminal_phase, restorable, updated_at_ns, host_inputs_json, \
    body_format, message_count, messages_len, checkpoints_len, config_len, snapshot_revision";

/// A header row as stored: `config_json` and `checkpoints_json` still hold tokens.
#[derive(Debug)]
pub(crate) struct HeaderRow {
    pub run_id: String,
    pub role: String,
    pub name: String,
    pub parent_run_id: Option<String>,
    pub config_json: String,
    pub checkpoints_json: String,
    pub terminal_phase: String,
    pub restorable: bool,
    pub updated_at_ns: i64,
    pub host_inputs_json: String,
    pub body_format: i64,
    pub message_count: i64,
    pub messages_len: i64,
    pub checkpoints_len: i64,
    pub config_len: i64,
    pub snapshot_revision: u64,
}

impl HeaderRow {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            run_id: row.get(0)?,
            role: row.get(1)?,
            name: row.get(2)?,
            parent_run_id: row.get(3)?,
            config_json: row.get(4)?,
            checkpoints_json: row.get(5)?,
            terminal_phase: row.get(6)?,
            restorable: row.get(7)?,
            updated_at_ns: row.get(8)?,
            host_inputs_json: row.get(9)?,
            body_format: row.get(10)?,
            message_count: row.get(11)?,
            messages_len: row.get(12)?,
            checkpoints_len: row.get(13)?,
            config_len: row.get(14)?,
            snapshot_revision: row.get(15)?,
        })
    }
}

fn corrupt(run_id: &str, what: impl std::fmt::Display) -> StorageError {
    StorageError::Serialization(format!("run context {run_id} is inconsistent: {what}"))
}

fn to_json(value: &Value) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| StorageError::Serialization(error.to_string()))
}

fn len_i64(len: usize, what: &'static str) -> Result<i64, StorageError> {
    i64::try_from(len).map_err(|_| StorageError::OutOfRange(what))
}

/// Runs `body` in one read snapshot: its own `BEGIN ... COMMIT` when the connection is in
/// autocommit mode, else inside the caller's transaction. Header, messages and blobs
/// read in it cannot come from two different commits.
pub(crate) fn read_snapshot<T>(
    conn: &Connection,
    body: impl FnOnce() -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    if !conn.is_autocommit() {
        return body();
    }
    conn.execute_batch("BEGIN")?;
    let result = body();
    let end = conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" });
    let value = result?;
    end?;
    Ok(value)
}

// ---------------------------------------------------------------- writing

/// The stored header's configuration as a document: parsed, so that the saved text is
/// the canonical serialization of the same JSON.
fn parse_config(config_json: &str) -> Result<Value, StorageError> {
    serde_json::from_str(config_json).map_err(|_| {
        StorageError::Serialization("run context configuration is not valid JSON".into())
    })
}

/// `non_restorable_reason` of a configuration document, when it is a string.
fn reason_of(config: &Value) -> Option<String> {
    config
        .get("non_restorable_reason")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Writes the header, the changed messages and the end of the conversation. The caller
/// holds the write transaction.
pub(crate) fn write_snapshot(
    conn: &Connection,
    record: &RunContextRecord,
    redacted: &RedactedSnapshot,
    snapshot_revision: u64,
) -> Result<(), StorageError> {
    let messages = redacted
        .messages
        .as_array()
        .ok_or_else(|| StorageError::Serialization("messages are not a JSON array".into()))?;
    // The per-message serialization is what is hashed; joined, it is the whole array.
    let plain: Vec<String> = messages
        .iter()
        .map(to_json)
        .collect::<Result<_, StorageError>>()?;
    debug_assert_eq!(
        format!("[{}]", plain.join(",")),
        redacted.messages_json,
        "messages must serialize as the join of their serializations"
    );
    write_header(conn, record, redacted, snapshot_revision, messages.len())?;
    write_messages(conn, &record.run_id, messages, &plain)
}

fn write_header(
    conn: &Connection,
    record: &RunContextRecord,
    redacted: &RedactedSnapshot,
    snapshot_revision: u64,
    message_count: usize,
) -> Result<(), StorageError> {
    let snapshot_revision = i64::try_from(snapshot_revision)
        .map_err(|_| StorageError::OutOfRange("snapshot revision"))?;
    let host_inputs_json =
        crate::repo::host_input_receipt::normalized_ids(&record.host_inputs_json)?;
    let config = parse_config(&record.config_json)?;
    let config_plain = to_json(&config)?;
    let reason = reason_of(&config);
    let messages_len = len_i64(redacted.messages_json.len(), "messages length")?;
    let checkpoints_len = len_i64(redacted.checkpoints_json.len(), "checkpoints length")?;
    let config_len = len_i64(config_plain.len(), "config length")?;
    let message_count = len_i64(message_count, "message count")?;
    let mut containers = [config, redacted.checkpoints.clone()];
    put_owner(
        conn,
        &HEADER_OWNER,
        &[SqlValue::Text(record.run_id.clone())],
        &mut containers,
        |conn, documents| {
            let [config, checkpoints] = documents else {
                return Err(StorageError::OutOfRange("header documents"));
            };
            conn.prepare_cached(
                "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, \
                 checkpoints_json, terminal_phase, restorable, updated_at_ns, snapshot_revision, \
                 host_inputs_json, messages_len, checkpoints_len, body_format, header_rev, \
                 message_count, config_len, non_restorable_reason) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 1, ?15, ?16, ?17) \
                 ON CONFLICT(run_id) DO UPDATE SET role = excluded.role, name = excluded.name, \
                 parent_run_id = excluded.parent_run_id, config_json = excluded.config_json, \
                 checkpoints_json = excluded.checkpoints_json, \
                 terminal_phase = excluded.terminal_phase, restorable = excluded.restorable, \
                 updated_at_ns = excluded.updated_at_ns, snapshot_revision = excluded.snapshot_revision, \
                 host_inputs_json = excluded.host_inputs_json, messages_len = excluded.messages_len, \
                 checkpoints_len = excluded.checkpoints_len, body_format = excluded.body_format, \
                 message_count = excluded.message_count, config_len = excluded.config_len, \
                 non_restorable_reason = excluded.non_restorable_reason, \
                 header_rev = header_rev + 1",
            )?
            .execute(params![
                record.run_id,
                record.role,
                record.name,
                record.parent_run_id,
                to_json(config)?,
                to_json(checkpoints)?,
                record.terminal_phase,
                record.restorable,
                record.updated_at_ns,
                snapshot_revision,
                host_inputs_json,
                messages_len,
                checkpoints_len,
                BODY_FORMAT,
                message_count,
                config_len,
                reason,
            ])?;
            Ok(())
        },
    )
}

/// What is stored for one ordinal of a run, without its body.
struct StoredMessage {
    id: i64,
    sha: Vec<u8>,
}

fn write_messages(
    conn: &Connection,
    run_id: &str,
    messages: &[Value],
    plain: &[String],
) -> Result<(), StorageError> {
    let existing: HashMap<i64, StoredMessage> = {
        let mut statement = conn
            .prepare_cached("SELECT ordinal, id, msg_sha256 FROM run_messages WHERE run_id = ?1")?;
        let rows = statement.query_map([run_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                StoredMessage {
                    id: row.get(1)?,
                    sha: row.get(2)?,
                },
            ))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    for (index, (message, text)) in messages.iter().zip(plain).enumerate() {
        let ordinal = len_i64(index, "message ordinal")?;
        let sha = Sha256::digest(text.as_bytes());
        let stored = existing.get(&ordinal);
        if stored.is_some_and(|stored| stored.sha == sha.as_slice()) {
            continue;
        }
        let id = match stored {
            Some(stored) => stored.id,
            None => next_message_id(conn)?,
        };
        let plain_len = len_i64(text.len(), "message length")?;
        let mut container = [message.clone()];
        let inserting = stored.is_none();
        put_owner(
            conn,
            &MESSAGE_OWNER,
            &[SqlValue::Integer(id)],
            &mut container,
            |conn, documents| {
                let skeleton = to_json(&documents[0])?;
                let (codec, bytes) = encode_text(&skeleton)?;
                if inserting {
                    conn.prepare_cached(
                        "INSERT INTO run_messages(id, run_id, ordinal, msg_sha256, plain_len, \
                         codec, skeleton) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    )?
                    .execute(params![
                        id,
                        run_id,
                        ordinal,
                        sha.as_slice(),
                        plain_len,
                        codec,
                        bytes
                    ])?;
                } else {
                    conn.prepare_cached(
                        "UPDATE run_messages SET msg_sha256 = ?2, plain_len = ?3, codec = ?4, \
                         skeleton = ?5 WHERE id = ?1",
                    )?
                    .execute(params![
                        id,
                        sha.as_slice(),
                        plain_len,
                        codec,
                        bytes
                    ])?;
                }
                Ok(())
            },
        )?;
    }
    // Rows past the new end: a shortened conversation (rewind, compaction).
    let end = len_i64(messages.len(), "message count")?;
    conn.prepare_cached("DELETE FROM run_messages WHERE run_id = ?1 AND ordinal >= ?2")?
        .execute(params![run_id, end])?;
    Ok(())
}

/// The id a new message row takes. The write transaction is held, so nobody else can
/// take it before the insert.
fn next_message_id(conn: &Connection) -> Result<i64, StorageError> {
    let max: i64 = conn
        .prepare_cached("SELECT COALESCE(MAX(id), 0) FROM run_messages")?
        .query_row([], |row| row.get(0))?;
    max.checked_add(1)
        .ok_or(StorageError::OutOfRange("message id"))
}

// ---------------------------------------------------------------- header updates

/// Stored header documents of `run_id`, parsed, tokens untouched.
struct StoredHeader {
    config: Value,
    checkpoints: Value,
    restorable: bool,
}

fn stored_header(conn: &Connection, run_id: &str) -> Result<Option<StoredHeader>, StorageError> {
    let row: Option<(String, String, bool)> = conn
        .prepare_cached(
            "SELECT config_json, checkpoints_json, restorable FROM run_contexts WHERE run_id = ?1",
        )?
        .query_row([run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .optional()?;
    let Some((config, checkpoints, restorable)) = row else {
        return Ok(None);
    };
    let parse = |text: &str, what: &str| {
        serde_json::from_str::<Value>(text)
            .map_err(|_| corrupt(run_id, format!("{what} is not valid JSON")))
    };
    Ok(Some(StoredHeader {
        config: parse(&config, "configuration")?,
        checkpoints: parse(&checkpoints, "checkpoint list")?,
        restorable,
    }))
}

/// `tokenize_in_place` that only computes tokens: stores nothing, so a comparison does
/// not leave blobs behind.
fn tokenize_without_storing(value: &mut Value) -> Result<(), StorageError> {
    tokenize_in_place(value, &mut |text| Ok(BodyToken::of(text)))
}

/// Replaces the stored configuration (tokens as they are) and the projections that
/// derive from it. `config` is in token form; `resolved_len` is its length with every
/// body resolved. The checkpoint list is not touched, but its tokens stay referenced.
fn replace_config(
    conn: &Connection,
    run_id: &str,
    config: &Value,
    checkpoints: &Value,
    resolved_len: usize,
    restorable: bool,
) -> Result<(), StorageError> {
    let reason = reason_of(config);
    let resolved_len = len_i64(resolved_len, "config length")?;
    put_owner_with_stored(
        conn,
        &HEADER_OWNER,
        &[SqlValue::Text(run_id.to_owned())],
        &mut [],
        &[config, checkpoints],
        |conn, _| {
            conn.prepare_cached(
                "UPDATE run_contexts SET config_json = ?2, config_len = ?3, \
                 non_restorable_reason = ?4, restorable = ?5, header_rev = header_rev + 1 \
                 WHERE run_id = ?1",
            )?
            .execute(params![
                run_id,
                to_json(config)?,
                resolved_len,
                reason,
                restorable
            ])?;
            Ok(())
        },
    )
}

fn resolved_length(conn: &Connection, run_id: &str, config: &Value) -> Result<usize, StorageError> {
    let mut resolved = config.clone();
    let owner = format!("run_header run={run_id}");
    resolve_in_place(&mut resolved, &owner, &mut |token| {
        load_blob(conn, token, &owner)
    })?;
    Ok(to_json(&resolved)?.len())
}

/// Compare-and-swap of a non-restorable run's configuration. `expected_config` is the
/// configuration as a reader got it (bodies resolved); it matches when its token form is
/// the stored one. Nothing is stored for a comparison that fails.
pub(crate) fn compare_and_swap_restore_state(
    conn: &Connection,
    run_id: &str,
    expected_config: &str,
    retry_config: &str,
    retry_restorable: bool,
) -> Result<bool, StorageError> {
    crate::repo::thread_index::atomic_write(conn, || {
        let Some(stored) = stored_header(conn, run_id)? else {
            return Ok(false);
        };
        if stored.restorable {
            return Ok(false);
        }
        // A stored configuration is always valid JSON, so one that is not cannot be it.
        let Ok(mut expected) = serde_json::from_str::<Value>(expected_config) else {
            return Ok(false);
        };
        tokenize_without_storing(&mut expected)?;
        if expected != stored.config {
            return Ok(false);
        }
        let retry = parse_config(retry_config)?;
        let retry_len = to_json(&retry)?.len();
        let reason = reason_of(&retry);
        let resolved_len = len_i64(retry_len, "config length")?;
        let mut containers = [retry];
        put_owner_with_stored(
            conn,
            &HEADER_OWNER,
            &[SqlValue::Text(run_id.to_owned())],
            &mut containers,
            &[&stored.checkpoints],
            |conn, documents| {
                conn.prepare_cached(
                    "UPDATE run_contexts SET config_json = ?2, config_len = ?3, \
                     non_restorable_reason = ?4, restorable = ?5, header_rev = header_rev + 1 \
                     WHERE run_id = ?1",
                )?
                .execute(params![
                    run_id,
                    to_json(&documents[0])?,
                    resolved_len,
                    reason,
                    retry_restorable
                ])?;
                Ok(())
            },
        )?;
        Ok(true)
    })
}

/// Marks a run non-restorable (`persist_failed`). Messages and the checkpoint list are
/// not read; only the configuration is edited, in its token form.
pub(crate) fn invalidate(conn: &Connection, run_id: &str) -> Result<(), StorageError> {
    crate::repo::thread_index::atomic_write(conn, || {
        let Some(stored) = stored_header(conn, run_id)? else {
            return Ok(());
        };
        let mut config = stored.config;
        let Some(object) = config.as_object_mut() else {
            return Err(corrupt(run_id, "configuration is not a JSON object"));
        };
        object.insert("restorable".into(), Value::Bool(false));
        object.insert(
            "non_restorable_reason".into(),
            Value::String("persist_failed".into()),
        );
        let resolved = resolved_length(conn, run_id, &config)?;
        replace_config(conn, run_id, &config, &stored.checkpoints, resolved, false)
    })
}

// ---------------------------------------------------------------- reading

fn resolve_document(
    conn: &Connection,
    run_id: &str,
    what: &str,
    text: &str,
    expected_len: i64,
) -> Result<String, StorageError> {
    let mut value: Value = serde_json::from_str(text)
        .map_err(|_| corrupt(run_id, format!("{what} is not valid JSON")))?;
    let owner = format!("run_header run={run_id}");
    resolve_in_place(&mut value, &owner, &mut |token| {
        load_blob(conn, token, &owner)
    })?;
    let resolved = to_json(&value)?;
    if i64::try_from(resolved.len()).ok() != Some(expected_len) {
        return Err(corrupt(
            run_id,
            format!(
                "{what} is {} bytes, the header says {expected_len}",
                resolved.len()
            ),
        ));
    }
    Ok(resolved)
}

/// Messages `range` (ordinals, System messages included) of `run_id`, each as the text it
/// was saved as. The range must lie inside the `message_count` messages of the header,
/// and every one of them must be there, in order.
pub(crate) fn load_messages(
    conn: &Connection,
    run_id: &str,
    message_count: usize,
    range: std::ops::Range<usize>,
) -> Result<Vec<String>, StorageError> {
    if range.end > message_count || range.start > range.end {
        return Err(StorageError::OutOfRange("message range"));
    }
    let mut statement = conn.prepare_cached(
        "SELECT ordinal, codec, skeleton, plain_len FROM run_messages \
         WHERE run_id = ?1 AND ordinal >= ?2 AND ordinal < ?3 ORDER BY ordinal",
    )?;
    let mut rows = statement.query(params![
        run_id,
        len_i64(range.start, "message ordinal")?,
        len_i64(range.end, "message ordinal")?
    ])?;
    let mut messages = Vec::with_capacity(range.len());
    let mut expected = range.start;
    while let Some(row) = rows.next()? {
        let ordinal: i64 = row.get(0)?;
        if usize::try_from(ordinal).ok() != Some(expected) {
            return Err(corrupt(
                run_id,
                format!("message {expected} is missing (found ordinal {ordinal})"),
            ));
        }
        let owner = format!("message run={run_id} ordinal={ordinal}");
        let codec: i64 = row.get(1)?;
        let ValueRef::Blob(skeleton) = row.get_ref(2)? else {
            return Err(unresolved(&owner, None, BodyUnresolvedReason::MalformedRow));
        };
        let mut message: Value =
            serde_json::from_str(&decode_skeleton(codec, skeleton, &owner)?)
                .map_err(|_| corrupt(run_id, format!("message {ordinal} is not valid JSON")))?;
        resolve_in_place(&mut message, &owner, &mut |token| {
            load_blob(conn, token, &owner)
        })?;
        let text = to_json(&message)?;
        let plain_len: i64 = row.get(3)?;
        if i64::try_from(text.len()).ok() != Some(plain_len) {
            return Err(corrupt(
                run_id,
                format!(
                    "message {ordinal} is {} bytes, its row says {plain_len}",
                    text.len()
                ),
            ));
        }
        messages.push(text);
        expected += 1;
    }
    if expected != range.end {
        return Err(corrupt(
            run_id,
            format!("message {expected} is missing (expected {message_count} messages)"),
        ));
    }
    Ok(messages)
}

/// The messages array of a run, as the exact text that was saved.
pub(crate) fn load_messages_json(
    conn: &Connection,
    header: &HeaderRow,
) -> Result<String, StorageError> {
    let count = usize::try_from(header.message_count)
        .map_err(|_| corrupt(&header.run_id, "negative message count"))?;
    let messages = load_messages(conn, &header.run_id, count, 0..count)?;
    let text = format!("[{}]", messages.join(","));
    if i64::try_from(text.len()).ok() != Some(header.messages_len) {
        return Err(corrupt(
            &header.run_id,
            format!(
                "messages are {} bytes, the header says {}",
                text.len(),
                header.messages_len
            ),
        ));
    }
    Ok(text)
}

/// A refusal for a header that is not in the format this build reads.
fn require_format(header: &HeaderRow) -> Result<(), StorageError> {
    if header.body_format == BODY_FORMAT {
        Ok(())
    } else {
        Err(StorageError::ConversionRequired {
            found: 0,
            detail: "a run header is not in the current body format",
        })
    }
}

/// The record of a header row, with its messages, configuration and checkpoints
/// resolved. The caller provides the read snapshot.
pub(crate) fn assemble(
    conn: &Connection,
    header: HeaderRow,
) -> Result<RunContextRecord, StorageError> {
    require_format(&header)?;
    let messages_json = load_messages_json(conn, &header)?;
    let config_json = resolve_document(
        conn,
        &header.run_id,
        "configuration",
        &header.config_json,
        header.config_len,
    )?;
    let checkpoints_json = resolve_document(
        conn,
        &header.run_id,
        "checkpoint list",
        &header.checkpoints_json,
        header.checkpoints_len,
    )?;
    Ok(RunContextRecord {
        run_id: header.run_id,
        role: header.role,
        name: header.name,
        parent_run_id: header.parent_run_id,
        config_json,
        messages_json,
        checkpoints_json,
        terminal_phase: header.terminal_phase,
        restorable: header.restorable,
        updated_at_ns: header.updated_at_ns,
        host_inputs_json: header.host_inputs_json,
    })
}

const NOT_TOMBSTONED: &str = "run_id NOT IN (SELECT run_id FROM run_tombstones)";

/// The header of a run that is not deleted by a retention.
pub(crate) fn header(conn: &Connection, run_id: &str) -> Result<Option<HeaderRow>, StorageError> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {HEADER_COLUMNS} FROM run_contexts WHERE run_id = ?1 AND {NOT_TOMBSTONED}"
        ))?
        .query_row([run_id], HeaderRow::from_row)
        .optional()?)
}

/// The header of a run, tombstoned or not, for the bundle reader that checks the
/// tombstone itself.
pub(crate) fn header_any(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<HeaderRow>, StorageError> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {HEADER_COLUMNS} FROM run_contexts WHERE run_id = ?1"
        ))?
        .query_row([run_id], HeaderRow::from_row)
        .optional()?)
}

pub(crate) fn get(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<RunContextRecord>, StorageError> {
    read_snapshot(conn, || {
        header(conn, run_id)?
            .map(|header| assemble(conn, header))
            .transpose()
    })
}

/// The record and snapshot revision of a run, ignoring tombstones (the caller has
/// checked them).
pub(crate) fn get_with_revision(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<(RunContextRecord, u64)>, StorageError> {
    read_snapshot(conn, || {
        header_any(conn, run_id)?
            .map(|header| {
                let revision = header.snapshot_revision;
                assemble(conn, header).map(|record| (record, revision))
            })
            .transpose()
    })
}

/// The records of `run_ids` that exist and are not deleted, in the order asked; a run
/// listed twice is read once.
pub(crate) fn records_for(
    conn: &Connection,
    ids_json: &str,
) -> Result<Vec<RunContextRecord>, StorageError> {
    read_snapshot(conn, || {
        let headers: Vec<HeaderRow> = {
            let mut statement = conn.prepare(&format!(
                "SELECT {} FROM json_each(?1) AS requested \
                 JOIN run_contexts ON run_contexts.run_id = requested.value \
                 WHERE requested.value NOT IN (SELECT run_id FROM run_tombstones) \
                 ORDER BY requested.key",
                HEADER_COLUMNS
                    .split(", ")
                    .map(|column| format!("run_contexts.{}", column.trim()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))?;
            let rows = statement.query_map([ids_json], HeaderRow::from_row)?;
            rows.collect::<Result<_, _>>()?
        };
        headers
            .into_iter()
            .map(|header| assemble(conn, header))
            .collect()
    })
}

/// The messages of a run saved as a checkpoint, for a chat that resumes from it.
/// `None` when the run is absent, deleted, or not a checkpoint.
pub(crate) fn checkpoint_messages(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<String>, StorageError> {
    read_snapshot(conn, || {
        let header: Option<HeaderRow> = conn
            .prepare_cached(&format!(
                "SELECT {HEADER_COLUMNS} FROM run_contexts \
                 WHERE run_id = ?1 AND terminal_phase = 'Checkpoint' AND {NOT_TOMBSTONED}"
            ))?
            .query_row([run_id], HeaderRow::from_row)
            .optional()?;
        let Some(header) = header else {
            return Ok(None);
        };
        require_format(&header)?;
        load_messages_json(conn, &header).map(Some)
    })
}

/// A header and the saved text of its messages `range`, for inspection. `None` range
/// reads them all.
pub(crate) fn message_range(
    conn: &Connection,
    run_id: &str,
    range: Option<std::ops::Range<usize>>,
) -> Result<Option<(HeaderRow, Vec<String>)>, StorageError> {
    read_snapshot(conn, || {
        let Some(header) = header(conn, run_id)? else {
            return Ok(None);
        };
        require_format(&header)?;
        let count = usize::try_from(header.message_count)
            .map_err(|_| corrupt(run_id, "negative message count"))?;
        let range = range.unwrap_or(0..count);
        let messages = load_messages(conn, run_id, count, range)?;
        Ok(Some((header, messages)))
    })
}

/// A config or checkpoint document of a header, bodies resolved.
pub(crate) fn resolved_document(
    conn: &Connection,
    run_id: &str,
    text: &str,
) -> Result<Value, StorageError> {
    let mut value: Value = serde_json::from_str(text)
        .map_err(|_| corrupt(run_id, "header document is not valid JSON"))?;
    let owner = format!("run_header run={run_id}");
    resolve_in_place(&mut value, &owner, &mut |token| {
        load_blob(conn, token, &owner)
    })?;
    Ok(value)
}

/// Fixtures that put a run into the database without the bundle API: no provenance
/// sidecar, no thread binding, no index row, and the snapshot revision the test picks. The
/// conversation itself is stored the way every writer stores it.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A saved-looking run with empty messages and checkpoints.
    pub(crate) fn record(run_id: &str) -> RunContextRecord {
        RunContextRecord {
            run_id: run_id.into(),
            role: "worker".into(),
            name: "worker".into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: "[]".into(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Done".into(),
            restorable: true,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        }
    }

    /// Inserts or replaces the run's header and messages in `conn`'s write transaction
    /// (one is started when there is none). Nothing is redacted.
    pub(crate) fn insert(conn: &Connection, record: &RunContextRecord, snapshot_revision: u64) {
        let messages: Value = serde_json::from_str(&record.messages_json).unwrap();
        let checkpoints: Value = serde_json::from_str(&record.checkpoints_json).unwrap();
        let redacted = RedactedSnapshot {
            checkpoint_count: checkpoints.as_array().map_or(0, Vec::len),
            messages_json: serde_json::to_string(&messages).unwrap(),
            checkpoints_json: serde_json::to_string(&checkpoints).unwrap(),
            messages,
            checkpoints,
        };
        crate::repo::thread_index::atomic_write(conn, || {
            write_snapshot(conn, record, &redacted, snapshot_revision)
        })
        .unwrap();
    }
}
