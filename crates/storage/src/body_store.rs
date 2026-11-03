//! Writing and reading content blobs, and the single entry point owners use to store a
//! row that holds tokens.
//!
//! # The write invariant
//!
//! A blob, the owner row that holds its token, and the owner's reference rows are written
//! by **one writer connection inside one `IMMEDIATE` transaction**. Every entry point
//! here refuses to run outside a write transaction ([`require_write_transaction`]), and
//! [`put_blob`] decides whether a blob exists by asking *inside* that transaction: what
//! another connection saw earlier is never a reason to skip the insert. That is what makes
//! the collector's delete safe: it also runs in a write transaction, so it either deletes
//! before this transaction starts (the blob is inserted again) or after it commits (the
//! blob is referenced and kept).
//!
//! # Owners
//!
//! An owner is a table whose rows hold JSON containers with tokens (`run_messages`,
//! `run_contexts`, tokenized `events`). Each has a reference table
//! `content_blob_refs_<owner>` listing the blobs one owner row names, created next to it
//! (see [`OwnerSpec::ref_table_ddl`] for the shape every such table must have). The
//! production owners are registered in [`PRODUCTION_OWNERS`] by the PR that creates them.

use std::collections::BTreeSet;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, TransactionState, params, params_from_iter};
use serde_json::Value;

use crate::StorageError;
use crate::body_ref::{BodyToken, collect_tokens, resolve_in_place, tokenize_in_place};
use crate::error::BodyUnresolvedReason;

/// The largest body a blob may hold: SQLite's own default limit for one value.
pub(crate) const MAX_BLOB_BYTES: u64 = 1_000_000_000;
pub(crate) const CODEC_PLAIN: i64 = 0;
pub(crate) const CODEC_ZSTD: i64 = 1;
const ZSTD_LEVEL: i32 = 3;
/// Name prefix of every reference table. The collector treats each table with this
/// prefix as a reference table, registered or not.
pub(crate) const REF_TABLE_PREFIX: &str = "content_blob_refs_";

pub(crate) fn unresolved(
    owner: &str,
    hash: Option<String>,
    reason: BodyUnresolvedReason,
) -> StorageError {
    StorageError::BodyUnresolved {
        owner: owner.to_owned(),
        hash,
        reason,
    }
}

/// Fails unless `conn` holds a write transaction (`BEGIN IMMEDIATE`, or a deferred one
/// that already wrote): the lock that serializes this writer against the collector.
pub(crate) fn require_write_transaction(conn: &Connection) -> Result<(), StorageError> {
    match conn.transaction_state(Some("main"))? {
        TransactionState::Write => Ok(()),
        _ => Err(StorageError::Serialization(
            "content blobs are written only inside an IMMEDIATE write transaction".into(),
        )),
    }
}

/// A blob that [`put_blob`] stored or found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoredBlob {
    pub id: i64,
    pub token: BodyToken,
    /// Whether this call inserted the row.
    pub created: bool,
}

/// Stores `text` (once per distinct content) and returns its row.
///
/// Must run in the write transaction that also writes the reference: see the module
/// documentation. An existing row with the same hash must have the same length; any
/// other length is a collision or corruption and fails with
/// [`BodyUnresolvedReason::HashLengthConflict`], on which the caller rolls back.
pub(crate) fn put_blob(conn: &Connection, text: &str) -> Result<StoredBlob, StorageError> {
    require_write_transaction(conn)?;
    let token = BodyToken::of(text);
    if token.byte_len > MAX_BLOB_BYTES {
        return Err(StorageError::OutOfRange("content blob bytes"));
    }
    if let Some(id) = find_existing(conn, &token)? {
        return Ok(StoredBlob {
            id,
            token,
            created: false,
        });
    }
    let (codec, data) = encode(text)?;
    let inserted = conn
        .prepare_cached(
            "INSERT INTO content_blobs(hash, byte_len, codec, data) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(hash) DO NOTHING",
        )?
        .execute(params![
            &token.hash[..],
            i64::try_from(token.byte_len).map_err(|_| StorageError::OutOfRange("blob length"))?,
            codec,
            data
        ])?;
    if inserted == 1 {
        return Ok(StoredBlob {
            id: conn.last_insert_rowid(),
            token,
            created: true,
        });
    }
    // Only another row with this hash makes the insert do nothing; take its id, after
    // checking it is the same text.
    let id = find_existing(conn, &token)?.ok_or_else(|| {
        unresolved(
            "content_blobs",
            Some(token.hash_hex()),
            BodyUnresolvedReason::BlobMissing,
        )
    })?;
    Ok(StoredBlob {
        id,
        token,
        created: false,
    })
}

fn find_existing(conn: &Connection, token: &BodyToken) -> Result<Option<i64>, StorageError> {
    let found: Option<(i64, i64)> = conn
        .prepare_cached("SELECT id, byte_len FROM content_blobs WHERE hash = ?1")?
        .query_row([&token.hash[..]], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    match found {
        Some((id, length)) if u64::try_from(length).ok() == Some(token.byte_len) => Ok(Some(id)),
        Some((_, length)) => Err(unresolved(
            "content_blobs",
            Some(token.hash_hex()),
            BodyUnresolvedReason::HashLengthConflict {
                stored: u64::try_from(length).unwrap_or(0),
                new: token.byte_len,
            },
        )),
        None => Ok(None),
    }
}

/// `(codec, stored bytes)`: zstd only when it is smaller.
pub(crate) fn encode_text(text: &str) -> Result<(i64, Vec<u8>), StorageError> {
    encode(text)
}

fn encode(text: &str) -> Result<(i64, Vec<u8>), StorageError> {
    let compressed = zstd::bulk::compress(text.as_bytes(), ZSTD_LEVEL)
        .map_err(|error| StorageError::Serialization(format!("zstd: {error}")))?;
    Ok(if compressed.len() < text.len() {
        (CODEC_ZSTD, compressed)
    } else {
        (CODEC_PLAIN, text.as_bytes().to_vec())
    })
}

/// The bytes of a `data` column value; `None` when it is neither a blob nor text (a
/// row altered outside this code).
pub(crate) fn blob_bytes(value: rusqlite::types::ValueRef<'_>) -> Option<&[u8]> {
    match value {
        rusqlite::types::ValueRef::Blob(bytes) | rusqlite::types::ValueRef::Text(bytes) => {
            Some(bytes)
        }
        _ => None,
    }
}

/// The text stored in a blob row, checked against the row's own declaration.
///
/// The declared length bounds the output: a compressed frame must declare exactly that
/// length before anything is allocated, and the decompressed bytes must match it. Hash
/// verification is [`verify_decoded`]'s job (it costs a full hash).
pub(crate) fn decode_blob(
    byte_len: i64,
    codec: i64,
    data: &[u8],
    owner: &str,
    hash: &str,
) -> Result<String, StorageError> {
    let fail = |reason| unresolved(owner, Some(hash.to_owned()), reason);
    let declared = u64::try_from(byte_len)
        .ok()
        .filter(|declared| *declared <= MAX_BLOB_BYTES)
        .ok_or_else(|| {
            fail(BodyUnresolvedReason::DeclaredLengthTooLarge {
                declared: u64::try_from(byte_len).unwrap_or(u64::MAX),
            })
        })?;
    let bytes = match codec {
        CODEC_PLAIN => data.to_vec(),
        CODEC_ZSTD => {
            let frame = zstd::zstd_safe::get_frame_content_size(data)
                .ok()
                .flatten()
                .ok_or_else(|| fail(BodyUnresolvedReason::DecompressFailed))?;
            if frame != declared {
                return Err(fail(BodyUnresolvedReason::LengthMismatch {
                    expected: declared,
                    actual: frame,
                }));
            }
            let capacity = usize::try_from(declared)
                .map_err(|_| fail(BodyUnresolvedReason::DecompressFailed))?;
            zstd::bulk::decompress(data, capacity)
                .map_err(|_| fail(BodyUnresolvedReason::DecompressFailed))?
        }
        other => return Err(fail(BodyUnresolvedReason::UnknownCodec(other))),
    };
    if bytes.len() as u64 != declared {
        return Err(fail(BodyUnresolvedReason::LengthMismatch {
            expected: declared,
            actual: bytes.len() as u64,
        }));
    }
    String::from_utf8(bytes).map_err(|_| fail(BodyUnresolvedReason::InvalidUtf8))
}

/// The text of a `run_messages.skeleton` (or any codec-tagged column that does not
/// record its plain length): a plain value, or one zstd frame whose declared content size
/// is the only thing the allocation follows, and is bounded by [`MAX_BLOB_BYTES`].
pub(crate) fn decode_skeleton(
    codec: i64,
    data: &[u8],
    owner: &str,
) -> Result<String, StorageError> {
    let fail = |reason| unresolved(owner, None, reason);
    let bytes = match codec {
        CODEC_PLAIN => data.to_vec(),
        CODEC_ZSTD => {
            let declared = zstd::zstd_safe::get_frame_content_size(data)
                .ok()
                .flatten()
                .ok_or_else(|| fail(BodyUnresolvedReason::DecompressFailed))?;
            if declared > MAX_BLOB_BYTES {
                return Err(fail(BodyUnresolvedReason::DeclaredLengthTooLarge {
                    declared,
                }));
            }
            let capacity = usize::try_from(declared)
                .map_err(|_| fail(BodyUnresolvedReason::DecompressFailed))?;
            let bytes = zstd::bulk::decompress(data, capacity)
                .map_err(|_| fail(BodyUnresolvedReason::DecompressFailed))?;
            if bytes.len() as u64 != declared {
                return Err(fail(BodyUnresolvedReason::LengthMismatch {
                    expected: declared,
                    actual: bytes.len() as u64,
                }));
            }
            bytes
        }
        other => return Err(fail(BodyUnresolvedReason::UnknownCodec(other))),
    };
    String::from_utf8(bytes).map_err(|_| fail(BodyUnresolvedReason::InvalidUtf8))
}

/// Fails with `HashMismatch` unless `text` hashes to `hash`.
pub(crate) fn verify_decoded(text: &str, hash: &[u8], owner: &str) -> Result<(), StorageError> {
    let token = BodyToken::of(text);
    if token.hash[..] == *hash {
        Ok(())
    } else {
        Err(unresolved(
            owner,
            Some(crate::body_ref::hex(hash)),
            BodyUnresolvedReason::HashMismatch,
        ))
    }
}

/// The body a token names. A missing blob, a length that differs from the token's, and
/// every defect of the stored row are `BodyUnresolved`.
pub(crate) fn load_blob(
    conn: &Connection,
    token: &BodyToken,
    owner: &str,
) -> Result<String, StorageError> {
    let hash = token.hash_hex();
    let row = conn
        .prepare_cached("SELECT byte_len, codec, data FROM content_blobs WHERE hash = ?1")?
        .query_row([&token.hash[..]], |row| {
            let byte_len: i64 = row.get(0)?;
            let codec: i64 = row.get(1)?;
            if u64::try_from(byte_len).ok() != Some(token.byte_len) {
                return Ok(Err(unresolved(
                    owner,
                    Some(hash.clone()),
                    BodyUnresolvedReason::LengthMismatch {
                        expected: token.byte_len,
                        actual: u64::try_from(byte_len).unwrap_or(u64::MAX),
                    },
                )));
            }
            Ok(match blob_bytes(row.get_ref(2)?) {
                Some(data) => decode_blob(byte_len, codec, data, owner, &hash),
                None => Err(unresolved(
                    owner,
                    Some(hash.clone()),
                    BodyUnresolvedReason::MalformedRow,
                )),
            })
        })
        .optional()?;
    row.unwrap_or_else(|| {
        Err(unresolved(
            owner,
            Some(hash.clone()),
            BodyUnresolvedReason::BlobMissing,
        ))
    })
}

/// Replaces every token of `container` with its body, reading blobs through `conn`.
/// `owner` labels errors. Any unresolvable token fails the whole call.
#[allow(dead_code)] // B1's readers call this; the header and message readers resolve per token.
pub(crate) fn resolve_container(
    conn: &Connection,
    owner: &str,
    container: &mut Value,
) -> Result<(), StorageError> {
    resolve_in_place(container, owner, &mut |token| load_blob(conn, token, owner))
}

/// One column of an owner's key, as named in the reference table and in the owner table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyColumn {
    pub ref_column: &'static str,
    pub owner_column: &'static str,
    /// SQL type in the reference table: `TEXT` or `INTEGER`.
    #[allow(dead_code)] // Read by `ref_table_ddl`, which the owners' PRs use.
    pub sql_type: &'static str,
}

/// Registry entry of one owner of blobs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnerSpec {
    pub name: &'static str,
    /// `content_blob_refs_<something>`.
    pub ref_table: &'static str,
    pub owner_table: &'static str,
    /// The owner row's key; the reference table's primary key is these plus `blob_id`.
    pub keys: &'static [KeyColumn],
    /// Columns of the owner table that hold JSON with tokens.
    pub container_columns: &'static [&'static str],
    /// Column holding the codec (0 plain, 1 zstd) of the container column, when the
    /// container is a codec-tagged BLOB instead of JSON text. Such an owner has exactly
    /// one container column.
    pub codec_column: Option<&'static str>,
    /// Condition selecting owner rows in token form (for a table where only some rows
    /// are), or `None` when every row is.
    pub filter: Option<&'static str>,
    /// Column of the owner table holding the run, for `check-bodies --run/--thread`.
    pub run_column: Option<&'static str>,
}

/// The owners registered in production. A2b and B1 add theirs; the collector does not
/// depend on this list (it honours every `content_blob_refs_*` table), the integrity
/// check does.
pub(crate) const PRODUCTION_OWNERS: &[OwnerSpec] = &[MESSAGE_OWNER, HEADER_OWNER];

/// One provider message of a run: `run_messages.skeleton`, a codec-tagged BLOB.
pub(crate) const MESSAGE_OWNER: OwnerSpec = OwnerSpec {
    name: "message",
    ref_table: "content_blob_refs_message",
    owner_table: "run_messages",
    keys: &[KeyColumn {
        ref_column: "message_id",
        owner_column: "id",
        sql_type: "INTEGER",
    }],
    container_columns: &["skeleton"],
    codec_column: Some("codec"),
    filter: None,
    run_column: Some("run_id"),
};

/// The header of a run: configuration and checkpoint list, JSON text in `run_contexts`.
pub(crate) const HEADER_OWNER: OwnerSpec = OwnerSpec {
    name: "run_header",
    ref_table: "content_blob_refs_header",
    owner_table: "run_contexts",
    keys: &[KeyColumn {
        ref_column: "run_id",
        owner_column: "run_id",
        sql_type: "TEXT",
    }],
    container_columns: &["config_json", "checkpoints_json"],
    codec_column: None,
    filter: None,
    run_column: Some("run_id"),
};

impl OwnerSpec {
    /// The statements that create this owner's reference table: owner FK with CASCADE,
    /// blob FK without action, index on `blob_id` (so a probe never scans), and the
    /// AFTER DELETE trigger that queues the blob **unconditionally**. Whether another
    /// owner still refers to it is decided only by the collector's delete statement.
    #[allow(dead_code)] // The owners (A2b/B1) compare their migrations with it; tests use it.
    pub(crate) fn ref_table_ddl(&self) -> String {
        let columns: Vec<String> = self
            .keys
            .iter()
            .map(|key| format!("{} {} NOT NULL", key.ref_column, key.sql_type))
            .collect();
        let ref_keys: Vec<&str> = self.keys.iter().map(|key| key.ref_column).collect();
        let owner_keys: Vec<&str> = self.keys.iter().map(|key| key.owner_column).collect();
        let table = self.ref_table;
        format!(
            "CREATE TABLE {table} (\n    {columns},\n    blob_id INTEGER NOT NULL REFERENCES content_blobs(id),\n    \
             PRIMARY KEY ({keys}, blob_id),\n    FOREIGN KEY ({keys}) REFERENCES {owner}({owner_keys}) ON DELETE CASCADE\n) WITHOUT ROWID;\n\
             CREATE INDEX idx_{table}_blob ON {table}(blob_id);\n\
             CREATE TRIGGER {table}_queue_gc AFTER DELETE ON {table}\nBEGIN\n    \
             INSERT OR IGNORE INTO content_blob_gc_queue(blob_id) VALUES (OLD.blob_id);\nEND;\n",
            columns = columns.join(",\n    "),
            keys = ref_keys.join(", "),
            owner = self.owner_table,
            owner_keys = owner_keys.join(", "),
        )
    }

    fn key_where(&self) -> String {
        self.keys
            .iter()
            .enumerate()
            .map(|(index, key)| format!("{} = ?{}", key.ref_column, index + 1))
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// A label for errors and reports: `name col=value, ...`.
    pub(crate) fn label(&self, key: &[SqlValue]) -> String {
        let mut label = self.name.to_owned();
        for (column, value) in self.keys.iter().zip(key) {
            let shown = match value {
                SqlValue::Integer(number) => number.to_string(),
                SqlValue::Text(text) => text.clone(),
                other => format!("{other:?}"),
            };
            label.push_str(&format!(" {}={shown}", column.ref_column));
        }
        label
    }
}

/// Stores one owner row and everything it needs, atomically, in the caller's write
/// transaction:
///
/// 1. every string of `containers` that needs a token is stored as a blob and replaced by
///    its token (so `containers` leave this call tokenized);
/// 2. `write_row` writes (INSERT or UPDATE) the owner row from the tokenized containers;
/// 3. the owner's reference rows are made equal to the set of blobs the containers name:
///    missing ones are inserted (`INSERT OR IGNORE`), ones no longer named are deleted
///    (which queues them for collection).
///
/// The three steps share a savepoint, so a failure in any of them leaves nothing of the
/// call behind even if the caller then commits. The owner row must exist after
/// `write_row`: the reference rows' foreign key points at it.
///
/// **`containers` are consumed.** They are rewritten in place and, after an `Err`, hold
/// an unspecified mix of original strings and tokens. A caller that retries must start
/// again from its own copy of the original values: tokenizing a container that already
/// holds tokens stores the token strings themselves as bodies, and they would read back
/// as tokens instead of as the original text.
///
/// Header-like owners that carry a revision counter must increment it in SQL
/// (`header_rev = header_rev + 1`) inside `write_row`, never from a value computed by
/// the client.
pub(crate) fn put_owner<R>(
    conn: &Connection,
    owner: &OwnerSpec,
    key: &[SqlValue],
    containers: &mut [Value],
    write_row: impl FnOnce(&Connection, &[Value]) -> Result<R, StorageError>,
) -> Result<R, StorageError> {
    put_owner_with_stored(conn, owner, key, containers, &[], write_row)
}

/// [`put_owner`] for a row that also keeps documents which are **already** in token form
/// (the stored checkpoint list that a header update leaves as it is). `stored` documents
/// are not tokenized again (that would store the tokens themselves as bodies): each token
/// in them must name a blob that exists now, in this transaction, with the declared
/// length, and joins the reference set. An unknown token or a missing blob is a hard
/// error, never a skipped reference. Only code inside this crate can reach this: a
/// document is never trusted because the caller says it holds tokens, the blobs have to
/// be there.
pub(crate) fn put_owner_with_stored<R>(
    conn: &Connection,
    owner: &OwnerSpec,
    key: &[SqlValue],
    containers: &mut [Value],
    stored: &[&Value],
    write_row: impl FnOnce(&Connection, &[Value]) -> Result<R, StorageError>,
) -> Result<R, StorageError> {
    require_write_transaction(conn)?;
    if key.len() != owner.keys.len() {
        return Err(StorageError::OutOfRange("owner key arity"));
    }
    conn.execute_batch("SAVEPOINT body_put_owner")?;
    let outcome = (|| {
        let mut blob_ids = BTreeSet::new();
        for container in containers.iter_mut() {
            tokenize_in_place(container, &mut |text| {
                let stored = put_blob(conn, text)?;
                blob_ids.insert(stored.id);
                Ok(stored.token)
            })?;
        }
        for document in stored {
            for token in collect_tokens(document, owner.name)? {
                blob_ids.insert(existing_blob_id(conn, &token, owner.name)?);
            }
        }
        let result = write_row(conn, containers)?;
        sync_refs(conn, owner, key, &blob_ids)?;
        Ok(result)
    })();
    match outcome {
        Ok(result) => {
            conn.execute_batch("RELEASE body_put_owner")?;
            Ok(result)
        }
        Err(error) => {
            // The rollback itself failing leaves the caller's transaction to be rolled
            // back by the caller; the original error is the one to report.
            let _ = conn.execute_batch("ROLLBACK TO body_put_owner; RELEASE body_put_owner");
            Err(error)
        }
    }
}

/// The id of the blob a token names; it must exist with the token's length.
fn existing_blob_id(
    conn: &Connection,
    token: &BodyToken,
    owner: &str,
) -> Result<i64, StorageError> {
    find_existing(conn, token)?.ok_or_else(|| {
        unresolved(
            owner,
            Some(token.hash_hex()),
            BodyUnresolvedReason::BlobMissing,
        )
    })
}

/// Makes the reference rows of `key` equal `blob_ids`.
fn sync_refs(
    conn: &Connection,
    owner: &OwnerSpec,
    key: &[SqlValue],
    blob_ids: &BTreeSet<i64>,
) -> Result<(), StorageError> {
    let table = owner.ref_table;
    let key_where = owner.key_where();
    let existing: BTreeSet<i64> = conn
        .prepare_cached(&format!("SELECT blob_id FROM {table} WHERE {key_where}"))?
        .query_map(params_from_iter(key.iter()), |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let blob_slot = owner.keys.len() + 1;
    let mut delete = conn.prepare_cached(&format!(
        "DELETE FROM {table} WHERE {key_where} AND blob_id = ?{blob_slot}"
    ))?;
    for removed in existing.difference(blob_ids) {
        let mut values: Vec<SqlValue> = key.to_vec();
        values.push(SqlValue::Integer(*removed));
        delete.execute(params_from_iter(values.iter()))?;
    }
    let columns: Vec<&str> = owner.keys.iter().map(|key| key.ref_column).collect();
    let slots: Vec<String> = (1..=blob_slot).map(|slot| format!("?{slot}")).collect();
    let mut insert = conn.prepare_cached(&format!(
        "INSERT OR IGNORE INTO {table}({}, blob_id) VALUES ({})",
        columns.join(", "),
        slots.join(", ")
    ))?;
    for added in blob_ids.difference(&existing) {
        let mut values: Vec<SqlValue> = key.to_vec();
        values.push(SqlValue::Integer(*added));
        insert.execute(params_from_iter(values.iter()))?;
    }
    Ok(())
}

#[cfg(test)]
mod process_tests;
#[cfg(test)]
pub(crate) mod test_owner;
#[cfg(test)]
mod tests;
