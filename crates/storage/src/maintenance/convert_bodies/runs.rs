//! One run's conversion: its messages become `run_messages` rows and blobs, its header
//! is updated in place, and the stored form is read back and compared with the old bytes
//! before the transaction commits.

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::body_store::{HEADER_OWNER, put_owner};
use crate::repo::run_messages::{
    BODY_FORMAT, assemble, header_any, len_i64, next_message_id, parse_config, put_message,
    reason_of, to_json,
};

use super::super::invalid;
use super::source::{Documents, MAX_RUN_BYTES, OldRun, documents, read_old_run};

/// Prefix of the errors that mean the stored form is not the old bytes.
pub(super) const MISMATCH: &str = "conversion mismatch";

pub(super) fn mismatch(run_id: &str, what: &str) -> StorageError {
    invalid(format!("{MISMATCH} in run {run_id}: {what}"))
}

/// A seam for tests: called inside the run's transaction at the named point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunPoint {
    BeforeReadback,
    BeforeCommit,
}

/// What converting one run stored.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Converted {
    pub messages: u64,
    /// Stored in the serializer's form, not as the old bytes (see `source::documents`).
    pub canonicalized: bool,
}

/// Converts `run_id` in one IMMEDIATE transaction. On any error nothing of the run is
/// stored (the transaction rolls back, blobs and references included).
pub(super) fn convert_run(
    conn: &Connection,
    run_id: &str,
    hook: &mut dyn FnMut(RunPoint, &Connection) -> Result<(), StorageError>,
) -> Result<Converted, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let old = read_old_run(&tx, run_id)?;
    let plain_bytes =
        old.messages.len() as u64 + old.checkpoints.len() as u64 + old.config.len() as u64;
    if plain_bytes > MAX_RUN_BYTES {
        return Err(invalid(format!(
            "run {run_id} holds {plain_bytes} plain bytes, above the {MAX_RUN_BYTES} the conversion handles"
        )));
    }
    let documents = documents(&old).map_err(|reason| mismatch(run_id, reason))?;
    let array = documents
        .messages
        .as_array()
        .ok_or_else(|| mismatch(run_id, "messages are not a JSON array"))?;
    let config = parse_config(&old.config)?;

    let first_id = next_message_id(&tx)?;
    for (index, (message, text)) in array.iter().zip(&documents.plain).enumerate() {
        let sha = Sha256::digest(text.as_bytes());
        let id = first_id
            .checked_add(len_i64(index, "message ordinal")?)
            .ok_or(StorageError::OutOfRange("message id"))?;
        put_message(
            &tx,
            run_id,
            len_i64(index, "message ordinal")?,
            id,
            true,
            message,
            text,
            sha.as_slice(),
        )?;
    }
    write_header(&tx, &old, &documents, config.clone())?;

    hook(RunPoint::BeforeReadback, &tx)?;
    read_back(&tx, &old.run_id, &documents, &config)?;
    hook(RunPoint::BeforeCommit, &tx)?;
    tx.commit()?;
    Ok(Converted {
        messages: array.len() as u64,
        canonicalized: documents.canonicalized,
    })
}

/// Updates the run's header row in place. It is not the normal header write: that one is
/// an upsert that does not name the old column, which is still `NOT NULL` here.
fn write_header(
    conn: &Connection,
    old: &OldRun,
    documents: &Documents,
    config: Value,
) -> Result<(), StorageError> {
    let message_count = documents.plain.len();
    let config_len = len_i64(to_json(&config)?.len(), "config length")?;
    let reason = reason_of(&config);
    let messages_len = len_i64(documents.messages_json.len(), "messages length")?;
    let checkpoints_len = len_i64(documents.checkpoints_json.len(), "checkpoints length")?;
    let message_count = len_i64(message_count, "message count")?;
    let mut containers = [config, documents.checkpoints.clone()];
    put_owner(
        conn,
        &HEADER_OWNER,
        &[SqlValue::Text(old.run_id.clone())],
        &mut containers,
        |conn, documents| {
            let [config, checkpoints] = documents else {
                return Err(StorageError::OutOfRange("header documents"));
            };
            let changed = conn
                .prepare_cached(
                    "UPDATE run_contexts SET config_json = ?2, checkpoints_json = ?3, \
                     messages_json = '', body_format = ?4, message_count = ?5, \
                     messages_len = ?6, checkpoints_len = ?7, config_len = ?8, \
                     non_restorable_reason = ?9, header_rev = header_rev + 1 WHERE run_id = ?1",
                )?
                .execute(params![
                    old.run_id,
                    to_json(config)?,
                    to_json(checkpoints)?,
                    BODY_FORMAT,
                    message_count,
                    messages_len,
                    checkpoints_len,
                    config_len,
                    reason,
                ])?;
            if changed != 1 {
                return Err(invalid(format!(
                    "run {} has {changed} header rows to update",
                    old.run_id
                )));
            }
            Ok(())
        },
    )
}

/// Reads the run through the normal reader and compares every document with the old one.
fn read_back(
    conn: &Connection,
    run_id: &str,
    documents: &Documents,
    config: &Value,
) -> Result<(), StorageError> {
    let header = header_any(conn, run_id)?
        .ok_or_else(|| mismatch(run_id, "the header is gone after the update"))?;
    let record = assemble(conn, header)
        .map_err(|error| mismatch(run_id, &format!("stored form unreadable: {error}")))?;
    if record.messages_json != documents.messages_json {
        return Err(mismatch(run_id, "messages differ after the round trip"));
    }
    if record.checkpoints_json != documents.checkpoints_json {
        return Err(mismatch(run_id, "checkpoints differ after the round trip"));
    }
    let stored: Value = serde_json::from_str(&record.config_json)
        .map_err(|_| mismatch(run_id, "configuration is not JSON after the round trip"))?;
    if &stored != config {
        return Err(mismatch(
            run_id,
            "configuration differs after the round trip",
        ));
    }
    Ok(())
}
