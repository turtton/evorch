//! Collection of content blobs nothing refers to any more.
//!
//! Every `content_blob_refs_*` table has an AFTER DELETE trigger that puts the blob of
//! the deleted row into `content_blob_gc_queue` **unconditionally**. The queue is only a
//! list of candidates: a blob in it may be referenced again by now (or by another owner
//! all along). What decides is the one statement that deletes, which tests every
//! reference table in the same statement and in the same write transaction:
//!
//! ```sql
//! DELETE FROM content_blobs WHERE id = ?1
//!   AND NOT EXISTS (SELECT 1 FROM <ref table> WHERE blob_id = ?1) AND ...
//! ```
//!
//! The reference tables are the tables named `content_blob_refs_*` found in the schema,
//! so a table nobody registered still protects its blobs. Foreign keys are the last line
//! of defence, not the check.
//!
//! [`gc_step`] runs on the writer's maintenance tick whether or not any retention job
//! exists. It is bounded in candidates, in deleted bytes, and in the size of one blob; a
//! blob above the single-blob limit is handed to the exclusive startup phase through
//! `reclaim_oversized`, which deletes it with the same statement.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::StorageError;
use crate::body_store::REF_TABLE_PREFIX;

mod check;
mod check_runs;

pub(crate) use check::{
    BlobReferences, BodyScope, blob_references, blob_state, blob_text, body_stats, check_bodies,
    find_blobs,
};
pub(crate) use check_runs::check_runs;

/// Queue entries one step looks at.
pub(crate) const GC_CANDIDATES_PER_STEP: usize = 64;
/// Stored bytes one step deletes, at most.
pub(crate) const GC_STORED_BYTES_PER_STEP: u64 = 8 * 1024 * 1024;
/// A blob with more stored bytes than this is never deleted by a tick.
pub(crate) const GC_SINGLE_BLOB_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// `family_key` of the oversized registrations made here. It is no tombstone family, so
/// no reclaim job waits for it.
pub(crate) const OVERSIZED_FAMILY: &str = "content-blobs";
/// `tbl` of those registrations.
pub(crate) const OVERSIZED_TABLE: &str = "content_blobs";

/// What one [`gc_step`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GcProgress {
    /// Queue entries looked at.
    pub candidates: usize,
    pub deleted: usize,
    pub deleted_bytes: u64,
    /// Entries whose blob is referenced (again) and was kept.
    pub retained: usize,
    /// Entries whose blob was already gone.
    pub stale: usize,
    /// Blobs handed to the exclusive phase.
    pub registered_oversized: usize,
    /// Whether entries are left for a later step because of the byte budget.
    pub deferred: bool,
}

/// The tables named `content_blob_refs_*`, in name order.
pub(crate) fn ref_tables(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let pattern = format!("{REF_TABLE_PREFIX}*");
    let tables: Vec<String> = conn
        .prepare_cached(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name GLOB ?1 ORDER BY name",
        )?
        .query_map([pattern], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(tables)
}

/// `AND NOT EXISTS (...)` for every reference table, on the blob `id_expr` names. Each
/// probe is a lookup in the table's `blob_id` index.
fn unreferenced_clause(tables: &[String], id_expr: &str) -> String {
    tables
        .iter()
        .map(|table| {
            format!(" AND NOT EXISTS (SELECT 1 FROM \"{table}\" WHERE blob_id = {id_expr})")
        })
        .collect()
}

/// Deletes blob `id` if no reference table refers to it; whether it did.
pub(crate) fn delete_unreferenced(
    conn: &Connection,
    tables: &[String],
    id: i64,
) -> Result<bool, StorageError> {
    let sql = format!(
        "DELETE FROM content_blobs WHERE id = ?1{}",
        unreferenced_clause(tables, "?1")
    );
    Ok(conn.prepare_cached(&sql)?.execute([id])? == 1)
}

/// Whether blob `id` exists and nothing refers to it.
fn is_unreferenced(conn: &Connection, tables: &[String], id: i64) -> Result<bool, StorageError> {
    let sql = format!(
        "SELECT 1 FROM content_blobs WHERE id = ?1{}",
        unreferenced_clause(tables, "?1")
    );
    Ok(conn
        .prepare_cached(&sql)?
        .query_row([id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// One bounded collection step. With nothing queued it only reads.
pub(crate) fn gc_step(conn: &Connection) -> Result<GcProgress, StorageError> {
    let queued = conn
        .prepare_cached("SELECT blob_id FROM content_blob_gc_queue LIMIT 1")?
        .query_row([], |_| Ok(()))
        .optional()?
        .is_some();
    if !queued {
        return Ok(GcProgress::default());
    }
    // The candidates are read inside the write transaction: what a reader saw before
    // the lock is not what the delete statement decides on.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let tables = ref_tables(&tx)?;
    let candidates: Vec<(i64, Option<u64>)> = tx
        .prepare_cached(
            "SELECT q.blob_id, OCTET_LENGTH(b.data) \
             FROM content_blob_gc_queue q LEFT JOIN content_blobs b ON b.id = q.blob_id \
             ORDER BY q.blob_id LIMIT ?1",
        )?
        .query_map([GC_CANDIDATES_PER_STEP], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut progress = GcProgress::default();
    for (id, stored) in candidates {
        progress.candidates += 1;
        match stored {
            None => progress.stale += 1,
            Some(stored) if stored > GC_SINGLE_BLOB_MAX_BYTES => {
                if is_unreferenced(&tx, &tables, id)? {
                    register_oversized(&tx, id, stored)?;
                    progress.registered_oversized += 1;
                } else {
                    progress.retained += 1;
                }
            }
            Some(stored) => {
                if progress.deleted_bytes.saturating_add(stored) > GC_STORED_BYTES_PER_STEP {
                    progress.candidates -= 1;
                    progress.deferred = true;
                    break;
                }
                if delete_unreferenced(&tx, &tables, id)? {
                    progress.deleted += 1;
                    progress.deleted_bytes += stored;
                } else {
                    progress.retained += 1;
                }
            }
        }
        tx.prepare_cached("DELETE FROM content_blob_gc_queue WHERE blob_id = ?1")?
            .execute([id])?;
    }
    tx.commit()?;
    Ok(progress)
}

fn register_oversized(conn: &Connection, id: i64, stored: u64) -> Result<(), StorageError> {
    conn.prepare_cached(
        "INSERT OR REPLACE INTO reclaim_oversized(family_key, tbl, key, stored_bytes) \
         VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![
        OVERSIZED_FAMILY,
        OVERSIZED_TABLE,
        id.to_string(),
        stored
    ])?;
    Ok(())
}

/// The exclusive phase's deletion of one registered blob, in the caller's transaction:
/// `(rows deleted, stored bytes deleted)`. The blob is deleted only if no reference
/// table refers to it now; a registration made earlier proves nothing. Its queue entry
/// goes with it.
pub(crate) fn delete_oversized(conn: &Connection, key: &str) -> Result<(u64, u64), StorageError> {
    let id: i64 = key
        .parse()
        .map_err(|_| StorageError::Serialization(format!("bad oversized blob id {key}")))?;
    let tables = ref_tables(conn)?;
    let stored: Option<u64> = conn
        .prepare_cached("SELECT OCTET_LENGTH(data) FROM content_blobs WHERE id = ?1")?
        .query_row([id], |row| row.get(0))
        .optional()?;
    let mut work = (0, 0);
    if let Some(stored) = stored
        && delete_unreferenced(conn, &tables, id)?
    {
        work = (1, stored);
    }
    conn.prepare_cached("DELETE FROM content_blob_gc_queue WHERE blob_id = ?1")?
        .execute([id])?;
    Ok(work)
}

#[cfg(test)]
mod tests;
