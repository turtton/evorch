//! `evorch inspect blob` and `evorch inspect check-bodies`: reading content blobs and
//! checking them against their owners. Read-only like the rest of inspection.

use serde_json::{Value, json};

use crate::body_ref::hex;
use crate::body_store::PRODUCTION_OWNERS;
use crate::repo::content_blobs::{
    BlobReferences, BodyScope, blob_references, blob_state, blob_text, check_bodies, check_runs,
    find_blobs,
};
use crate::{Database, StorageError};

/// Hex digits of a hash a lookup needs, so a prefix names one blob in practice.
pub const MIN_BLOB_PREFIX_DIGITS: usize = 12;
/// Bytes of a body shown unless the whole body is asked for.
pub const BLOB_PREVIEW_BYTES: usize = 2_048;
/// Reference rows listed per reference table unless asked otherwise.
pub const DEFAULT_REFERENCES_LISTED: usize = 20;
/// Most reference rows one call lists per reference table.
pub const MAX_REFERENCES_LISTED: usize = 1_000;

/// Which owner rows `check-bodies` looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyCheckScope {
    All,
    Run(String),
    Thread(String),
}

/// What `inspect blob` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobOptions {
    /// The whole body instead of its first [`BLOB_PREVIEW_BYTES`] bytes.
    pub full_content: bool,
    /// Reference rows to skip in each reference table, to page through a widely shared
    /// blob.
    pub reference_offset: usize,
    /// Reference rows to list per reference table, clamped to
    /// `1..=`[`MAX_REFERENCES_LISTED`].
    pub reference_limit: usize,
}

impl Default for BlobOptions {
    fn default() -> Self {
        Self {
            full_content: false,
            reference_offset: 0,
            reference_limit: DEFAULT_REFERENCES_LISTED,
        }
    }
}

impl Database {
    /// One blob by hash prefix: size, codec, who refers to it (a page of each reference
    /// table, with `next_offset` when more follow), whether it waits for collection, and
    /// its body (the first [`BLOB_PREVIEW_BYTES`] bytes unless `full_content`).
    ///
    /// # Errors
    /// Fails for a prefix shorter than [`MIN_BLOB_PREFIX_DIGITS`] or not lowercase hex,
    /// for no match, and for an ambiguous prefix. A body that cannot be decoded is
    /// reported in the result (`content_error`), not as a failure.
    pub fn inspect_blob(&self, prefix: &str, options: BlobOptions) -> Result<Value, StorageError> {
        let full_content = options.full_content;
        let limit = options.reference_limit.clamp(1, MAX_REFERENCES_LISTED);
        if prefix.len() < MIN_BLOB_PREFIX_DIGITS {
            return Err(StorageError::Serialization(format!(
                "a blob hash prefix needs at least {MIN_BLOB_PREFIX_DIGITS} hex digits"
            )));
        }
        let transaction = self.conn.unchecked_transaction()?;
        let conn: &rusqlite::Connection = &transaction;
        let found = find_blobs(conn, prefix)?;
        let meta = match found.as_slice() {
            [] => {
                return Err(StorageError::Serialization(format!(
                    "no blob with hash prefix {prefix}"
                )));
            }
            [only] => only,
            [first, second] => {
                return Err(StorageError::Serialization(format!(
                    "hash prefix {prefix} is ambiguous: it matches {} and {}",
                    hex(&first.hash),
                    hex(&second.hash)
                )));
            }
            _ => unreachable!("find_blobs returns at most two rows"),
        };
        let (queued, registered) = blob_state(conn, meta.id)?;
        let references = blob_references(conn, meta.id, options.reference_offset, limit)?;
        let mut result = json!({
            "hash": hex(&meta.hash),
            "id": meta.id,
            "byte_len": meta.byte_len,
            "stored_bytes": meta.stored_bytes,
            "codec": match meta.codec { 0 => "plain", 1 => "zstd", _ => "unknown" },
            "referenced_by": references
                .iter()
                .map(|(table, rows, owners): &BlobReferences| {
                    let end = options.reference_offset.saturating_add(owners.len());
                    json!({
                        "table": table,
                        "rows": rows,
                        "offset": options.reference_offset,
                        "owners": owners,
                        "next_offset": (i64::try_from(end).is_ok_and(|end| end < *rows)).then_some(end),
                    })
                })
                .collect::<Vec<_>>(),
            "waiting_for_collection": references.is_empty() && (queued || registered),
            "queued_for_collection": queued,
            "registered_for_exclusive_phase": registered,
        });
        match blob_text(conn, meta) {
            Ok(text) => {
                let shown = if full_content {
                    text.len()
                } else {
                    floor_char_boundary(&text, BLOB_PREVIEW_BYTES)
                };
                result["content"] = json!({
                    "truncated": shown < text.len(),
                    "shown_bytes": shown,
                    "text": &text[..shown],
                });
            }
            Err(error) => result["content_error"] = json!(error.to_string()),
        }
        Ok(result)
    }

    /// Checks every owner of blobs against its reference rows, and the blobs against
    /// the owners; `verify` also decompresses and hashes each blob read. The result's
    /// `ok` is false when any problem was found: the caller reports that as a failure.
    ///
    /// # Errors
    /// Fails when SQLite cannot be read; findings are part of the result.
    pub fn inspect_check_bodies(
        &self,
        scope: &BodyCheckScope,
        verify: bool,
    ) -> Result<Value, StorageError> {
        let scope_ref = match scope {
            BodyCheckScope::All => BodyScope::All,
            BodyCheckScope::Run(run) => BodyScope::Run(run),
            BodyCheckScope::Thread(thread) => BodyScope::Thread(thread),
        };
        let mut report = check_bodies(&self.conn, PRODUCTION_OWNERS, scope_ref, verify)?;
        check_runs(&self.conn, scope_ref, verify, &mut report)?;
        Ok(json!({
            "ok": report.is_ok(),
            "scope": match scope {
                BodyCheckScope::All => json!("all"),
                BodyCheckScope::Run(run) => json!({ "run_id": run }),
                BodyCheckScope::Thread(thread) => json!({ "thread_id": thread }),
            },
            "verified": verify,
            "checked": {
                "owners": report.owners_checked,
                "owner_rows": report.owner_rows,
                "tokens": report.tokens,
                "blobs_verified": report.blobs_verified,
                "reference_tables": report.ref_tables,
            },
            "waiting_for_collection": report.waiting_for_collection,
            "problem_count": report.problem_count,
            "problems": report
                .problems
                .iter()
                .map(|problem| json!({
                    "kind": problem.kind,
                    "owner": problem.owner,
                    "hash": problem.hash,
                    "detail": problem.detail,
                }))
                .collect::<Vec<_>>(),
            "problems_truncated": report.problem_count > report.problems.len() as u64,
        }))
    }
}

/// The largest index `<= limit` on a character boundary of `text`.
fn floor_char_boundary(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests;
