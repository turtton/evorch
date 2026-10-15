//! Durable assistant references, with consumption owned by the evidence transaction.

use rusqlite::{Connection, OptionalExtension, params};

use crate::{AssistantReferenceRow, StorageError};

pub fn save(conn: &Connection, row: &AssistantReferenceRow) -> Result<(), StorageError> {
    row.validate()?;
    if let Some(existing) = get(conn, &row.reference_id)? {
        if existing.source_run_id != row.source_run_id
            || existing.turn_end != row.turn_end
            || existing.text != row.text
            || existing.created_at_ns != row.created_at_ns
        {
            return Err(StorageError::AssistantReferenceConflict);
        }
        return Ok(());
    }
    if row.consumed {
        return Err(StorageError::AssistantReferenceUnavailable);
    }
    let turn_end = i64::try_from(row.turn_end)
        .map_err(|_| StorageError::OutOfRange("assistant reference turn end"))?;
    conn.execute(
        "INSERT INTO assistant_references (reference_id, source_run_id, turn_end, text, consumed, created_at_ns) VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        params![row.reference_id, row.source_run_id, turn_end, row.text, row.created_at_ns],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<AssistantReferenceRow>, StorageError> {
    Ok(conn.query_row(
        "SELECT reference_id, source_run_id, turn_end, text, consumed, created_at_ns FROM assistant_references WHERE reference_id = ?1",
        [id],
        |row| {
            let turn_end: i64 = row.get(2)?;
            Ok(AssistantReferenceRow {
                reference_id: row.get(0)?,
                source_run_id: row.get(1)?,
                turn_end: usize::try_from(turn_end)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, turn_end))?,
                text: row.get(3)?,
                consumed: row.get(4)?,
                created_at_ns: row.get(5)?,
            })
        },
    ).optional()?)
}

pub fn consume(conn: &Connection, id: Option<&str>) -> Result<(), StorageError> {
    let Some(id) = id else {
        return Ok(());
    };
    let changed = conn.execute(
        "UPDATE assistant_references SET consumed = 1 WHERE reference_id = ?1 AND consumed = 0",
        [id],
    )?;
    if changed != 1 {
        return Err(StorageError::AssistantReferenceUnavailable);
    }
    Ok(())
}
