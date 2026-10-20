//! Evaluation samples keyed by thread.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::StorageError;
use crate::eval_sample::EvalSample;

/// Insert a sample, or replace the problem and focus roles of the thread's
/// existing sample while keeping its creation time.
pub fn save(conn: &Connection, sample: &EvalSample) -> Result<(), StorageError> {
    sample.validate()?;
    let focus_roles = serde_json::to_string(&sample.focus_roles)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    conn.execute(
        "INSERT INTO eval_samples (thread_id, problem, focus_roles, created_at_ns, updated_at_ns) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(thread_id) DO UPDATE SET \
         problem = excluded.problem, focus_roles = excluded.focus_roles, \
         updated_at_ns = excluded.updated_at_ns",
        params![
            sample.thread_id,
            sample.problem,
            focus_roles,
            sample.created_at_ns,
            sample.updated_at_ns,
        ],
    )?;
    Ok(())
}

/// Returns whether a sample was deleted.
pub fn delete(conn: &Connection, thread_id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM eval_samples WHERE thread_id = ?1", [thread_id])? == 1)
}

pub fn get(conn: &Connection, thread_id: &str) -> Result<Option<EvalSample>, StorageError> {
    conn.query_row(
        "SELECT thread_id, problem, focus_roles, created_at_ns, updated_at_ns \
         FROM eval_samples WHERE thread_id = ?1",
        [thread_id],
        raw,
    )
    .optional()?
    .map(decode)
    .transpose()
}

/// Most recently updated first.
pub fn list(conn: &Connection) -> Result<Vec<EvalSample>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT thread_id, problem, focus_roles, created_at_ns, updated_at_ns \
         FROM eval_samples ORDER BY updated_at_ns DESC, thread_id",
    )?;
    let rows = statement.query_map([], raw)?;
    rows.map(|row| decode(row?)).collect()
}

type RawSample = (String, String, String, i64, i64);

fn raw(row: &Row<'_>) -> rusqlite::Result<RawSample> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn decode(
    (thread_id, problem, focus_roles, created_at_ns, updated_at_ns): RawSample,
) -> Result<EvalSample, StorageError> {
    Ok(EvalSample {
        thread_id,
        problem,
        focus_roles: serde_json::from_str(&focus_roles)
            .map_err(|error| StorageError::Serialization(error.to_string()))?,
        created_at_ns,
        updated_at_ns,
    })
}
