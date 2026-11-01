//! SQL of the artifact reference tables (see `crate::artifact_refs`).

use rusqlite::{Connection, params};

use crate::StorageError;
use crate::artifact_refs::{ArtifactRef, GcState, MetaState, UnknownMeta};

pub(crate) fn gc_state(conn: &Connection) -> Result<GcState, StorageError> {
    Ok(conn.query_row(
        "SELECT meta_cursor, blob_cursor, ref_cursor, refs_complete, meta_count, meta_hash \
         FROM gc_state WHERE id = 1",
        [],
        |row| {
            Ok(GcState {
                meta_cursor: row.get(0)?,
                blob_cursor: row.get(1)?,
                ref_cursor: row.get(2)?,
                refs_complete: row.get::<_, i64>(3)? != 0,
                meta_count: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                meta_hash: row.get(5)?,
            })
        },
    )?)
}

pub(crate) fn set_gc_state(conn: &Connection, state: &GcState) -> Result<(), StorageError> {
    let changed = conn.execute(
        "UPDATE gc_state SET meta_cursor = ?1, blob_cursor = ?2, ref_cursor = ?3, \
         refs_complete = ?4, meta_count = ?5, meta_hash = ?6 WHERE id = 1",
        params![
            state.meta_cursor,
            state.blob_cursor,
            state.ref_cursor,
            i64::from(state.refs_complete),
            i64::try_from(state.meta_count)
                .map_err(|_| StorageError::OutOfRange("gc meta count"))?,
            state.meta_hash
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StorageError::Serialization(
            "gc_state row is missing".to_owned(),
        ))
    }
}

pub(crate) fn ref_exists(conn: &Connection, artifact_id: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_refs WHERE artifact_id = ?1)",
        [artifact_id],
        |row| row.get(0),
    )?)
}

pub(crate) fn sha_referenced(conn: &Connection, sha256: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_refs WHERE sha256 = ?1)",
        [sha256],
        |row| row.get(0),
    )?)
}

pub(crate) fn meta_state(conn: &Connection, artifact_id: &str) -> Result<MetaState, StorageError> {
    let (referenced, unknown): (bool, bool) = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_refs WHERE artifact_id = ?1), \
                EXISTS(SELECT 1 FROM gc_unknown_meta WHERE name = ?1 || '.json')",
        [artifact_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(if referenced {
        MetaState::Referenced
    } else if unknown {
        MetaState::Unknown
    } else {
        MetaState::Missing
    })
}

pub(crate) fn insert_refs(conn: &Connection, refs: &[ArtifactRef]) -> Result<(), StorageError> {
    if refs.is_empty() {
        return Ok(());
    }
    let transaction = conn.unchecked_transaction()?;
    {
        let mut statement = transaction.prepare_cached(
            "INSERT OR REPLACE INTO artifact_refs \
             (artifact_id, sha256, thread_id, run_id, recorded_at_ns) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for item in refs {
            statement.execute(params![
                item.artifact_id,
                item.sha256,
                item.thread_id,
                item.run_id,
                item.recorded_at_ns
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

pub(crate) fn delete_refs(
    conn: &Connection,
    artifact_ids: &[String],
) -> Result<usize, StorageError> {
    if artifact_ids.is_empty() {
        return Ok(0);
    }
    let transaction = conn.unchecked_transaction()?;
    let mut removed = 0;
    {
        let mut statement =
            transaction.prepare_cached("DELETE FROM artifact_refs WHERE artifact_id = ?1")?;
        for id in artifact_ids {
            removed += statement.execute([id])?;
        }
    }
    transaction.commit()?;
    Ok(removed)
}

pub(crate) fn refs_after(
    conn: &Connection,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Vec<ArtifactRef>, StorageError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = conn.prepare_cached(
        "SELECT artifact_id, sha256, thread_id, run_id, recorded_at_ns FROM artifact_refs \
         WHERE artifact_id > ?1 ORDER BY artifact_id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![cursor.unwrap_or(""), limit], |row| {
        Ok(ArtifactRef {
            artifact_id: row.get(0)?,
            sha256: row.get(1)?,
            thread_id: row.get(2)?,
            run_id: row.get(3)?,
            recorded_at_ns: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub(crate) fn unknown_upsert(
    conn: &Connection,
    name: &str,
    error: &str,
    now_ns: i64,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO gc_unknown_meta (name, first_seen_ns, last_error) VALUES (?1, ?2, ?3) \
         ON CONFLICT(name) DO UPDATE SET last_error = excluded.last_error",
        params![name, now_ns, error],
    )?;
    Ok(())
}

pub(crate) fn unknown_delete(conn: &Connection, name: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM gc_unknown_meta WHERE name = ?1", [name])? > 0)
}

/// The rows that still block the sweep: a row whose artifact has a reference since
/// (a crash between the two writes of a resolution) blocks nothing.
const STILL_UNKNOWN: &str = "NOT EXISTS (SELECT 1 FROM artifact_refs r \
     WHERE r.artifact_id = substr(u.name, 1, length(u.name) - 5))";

pub(crate) fn unknown_list(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<UnknownMeta>, StorageError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = conn.prepare_cached(&format!(
        "SELECT u.name, u.first_seen_ns, u.last_error FROM gc_unknown_meta u \
         WHERE {STILL_UNKNOWN} ORDER BY u.name LIMIT ?1"
    ))?;
    let rows = statement.query_map([limit], |row| {
        Ok(UnknownMeta {
            name: row.get(0)?,
            first_seen_ns: row.get(1)?,
            last_error: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub(crate) fn unknown_count(conn: &Connection) -> Result<u64, StorageError> {
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM gc_unknown_meta u WHERE {STILL_UNKNOWN}"),
        [],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}
