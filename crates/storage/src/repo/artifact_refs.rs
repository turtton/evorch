//! SQL of the artifact reference tables (see `crate::artifact_refs`).

use rusqlite::{Connection, OptionalExtension, params};

use crate::StorageError;
use crate::artifact_refs::{ArtifactRef, GcState, MetaState, UnknownMeta};

const THREAD_REF: &str = "SELECT artifact_id, sha256, thread_id, run_id, recorded_at_ns \
    FROM artifact_refs WHERE thread_id = ?1 AND artifact_id = ?2";
const THREAD_PAGE: &str = "SELECT artifact_id, sha256, thread_id, run_id, recorded_at_ns \
    FROM artifact_refs WHERE thread_id = ?1 AND artifact_id > ?2 \
    ORDER BY artifact_id LIMIT ?3";
// LIMIT must be inside the subquery: an outer LIMIT would still count every row.
const THREAD_COUNT: &str = "SELECT count(*) FROM \
    (SELECT 1 FROM artifact_refs WHERE thread_id = ?1 LIMIT ?2)";

fn read_ref(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRef> {
    Ok(ArtifactRef {
        artifact_id: row.get(0)?,
        sha256: row.get(1)?,
        thread_id: row.get(2)?,
        run_id: row.get(3)?,
        recorded_at_ns: row.get(4)?,
    })
}

pub(crate) fn ref_for_thread(
    conn: &Connection,
    thread_id: &str,
    artifact_id: &str,
) -> Result<Option<ArtifactRef>, StorageError> {
    Ok(conn
        .query_row(THREAD_REF, params![thread_id, artifact_id], read_ref)
        .optional()?)
}

pub(crate) fn refs_for_thread(
    conn: &Connection,
    thread_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Vec<ArtifactRef>, StorageError> {
    let mut statement = conn.prepare_cached(THREAD_PAGE)?;
    let rows = statement.query_map(
        params![thread_id, cursor.unwrap_or(""), limit as i64],
        read_ref,
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn count_for_thread(
    conn: &Connection,
    thread_id: &str,
    limit: usize,
) -> Result<usize, StorageError> {
    Ok(
        conn.query_row(THREAD_COUNT, params![thread_id, limit as i64], |row| {
            row.get(0)
        })?,
    )
}

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

#[cfg(test)]
mod renderer_read_tests {
    use super::*;
    use crate::{Database, StorageConfig};
    use rusqlite::{StatementStatus, ToSql};

    fn indexed_steps(conn: &Connection, sql: &str, args: &[&dyn ToSql]) -> i32 {
        let plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(args, |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|step| step.contains("SEARCH") && step.contains("thread_id=?")),
            "{plan:?}"
        );
        if sql == THREAD_PAGE {
            assert!(
                plan.iter().any(|step| step.contains("artifact_id>?")),
                "{plan:?}"
            );
        }
        let mut statement = conn.prepare(sql).unwrap();
        let mut rows = statement.query(args).unwrap();
        while rows.next().unwrap().is_some() {}
        drop(rows);
        assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
        assert_eq!(statement.get_status(StatementStatus::Sort), 0);
        statement.get_status(StatementStatus::VmStep)
    }

    #[test]
    fn renderer_reads_seek_and_count_work_stops_at_the_requested_cap() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("refs.db"),
            ..Default::default()
        };
        let db = Database::open(&config).unwrap();
        let conn = Connection::open(&config.db_path).unwrap();
        conn.execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2048)
             INSERT INTO artifact_refs
             SELECT printf('m-%05d', x), 'shared', 'owned', NULL, x FROM n;",
        )
        .unwrap();
        assert_eq!(db.artifact_ref_count_for_thread("owned", 7).unwrap(), 7);
        let page_args: &[&dyn ToSql] = &[&"owned", &"m-00003", &7_i64];
        let count_args: &[&dyn ToSql] = &[&"owned", &7_i64];
        let first = indexed_steps(&conn, THREAD_PAGE, page_args);
        let count = indexed_steps(&conn, THREAD_COUNT, count_args);
        conn.execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<20000)
             INSERT INTO artifact_refs
             SELECT printf('a-%05d', x), 'shared', 'foreign', NULL, x FROM n;
             WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<20000)
             INSERT INTO artifact_refs
             SELECT printf('z-%05d', x), 'shared', 'owned', NULL, x FROM n;",
        )
        .unwrap();
        assert_eq!(indexed_steps(&conn, THREAD_PAGE, page_args), first);
        assert_eq!(indexed_steps(&conn, THREAD_COUNT, count_args), count);
        assert_eq!(db.artifact_ref_count_for_thread("owned", 7).unwrap(), 7);
    }
}
