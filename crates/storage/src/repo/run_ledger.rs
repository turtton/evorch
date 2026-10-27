use std::time::SystemTime;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::entity::SecretGuard;
use crate::{RunLedgerEntry, StorageError, system_time_to_ns};

/// Appends a note revision and advances the run's index source in one
/// transaction (ADR 0041 R1/R2), so no note write bypasses the index state.
pub fn append(conn: &Connection, run_id: &str, body: &str) -> Result<u64, StorageError> {
    let created_at_ns = system_time_to_ns(SystemTime::now())?;
    crate::repo::thread_index::append_note(conn, run_id, body, created_at_ns)
        .map(|receipt| receipt.note.seq)
}

/// Raw ledger insert; callers must also record the source change.
pub(crate) fn append_at(
    conn: &Connection,
    run_id: &str,
    body: &str,
    created_at_ns: i64,
) -> Result<u64, StorageError> {
    if body.len() > 8192 {
        return Err(StorageError::InvalidRunLedgerBody { bytes: body.len() });
    }
    SecretGuard::from_env().check_text("ledger", "body", body)?;
    let seq: i64 = conn.query_row(
        "INSERT INTO run_ledger(run_id, body, created_at_ns) VALUES (?1, ?2, ?3) RETURNING seq",
        params![run_id, body, created_at_ns],
        |row| row.get(0),
    )?;
    u64::try_from(seq).map_err(|_| StorageError::OutOfRange("run ledger sequence"))
}

pub fn latest(conn: &Connection, run_id: &str) -> Result<Option<RunLedgerEntry>, StorageError> {
    Ok(conn.query_row(
        "SELECT seq, run_id, body, created_at_ns FROM run_ledger WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
        [run_id],
        from_row,
    ).optional()?)
}

pub fn list_by_run(conn: &Connection, run_id: &str) -> Result<Vec<RunLedgerEntry>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT seq, run_id, body, created_at_ns FROM run_ledger WHERE run_id = ?1 ORDER BY seq",
    )?;
    Ok(statement
        .query_map([run_id], from_row)?
        .collect::<Result<_, _>>()?)
}

pub fn list_all(conn: &Connection) -> Result<Vec<RunLedgerEntry>, StorageError> {
    let mut statement =
        conn.prepare("SELECT seq, run_id, body, created_at_ns FROM run_ledger ORDER BY seq")?;
    Ok(statement
        .query_map([], from_row)?
        .collect::<Result<_, _>>()?)
}

pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<RunLedgerEntry> {
    Ok(RunLedgerEntry {
        seq: {
            let seq: i64 = row.get(0)?;
            u64::try_from(seq).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, seq))?
        },
        run_id: row.get(1)?,
        body: row.get(2)?,
        created_at_ns: row.get(3)?,
    })
}
