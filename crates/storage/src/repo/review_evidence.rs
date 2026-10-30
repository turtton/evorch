//! Guarded, idempotent review evidence. The writer owns transaction boundaries.

use rusqlite::{Connection, Row, params};

use crate::{ReviewEvidenceRow, StorageError};

const COLUMNS: &str = "id, root_run_id, target_run_id, kind, text, in_reply_to_question_id, \
                      in_reply_to_title, reference_kind, reference_context, created_at_ns, \
                      attachments_json";

pub fn insert(conn: &Connection, rows: &[ReviewEvidenceRow]) -> Result<(), StorageError> {
    let mut statement = conn.prepare(
        "INSERT OR IGNORE INTO review_evidence \
         (id, root_run_id, target_run_id, kind, text, in_reply_to_question_id, \
          in_reply_to_title, reference_kind, reference_context, created_at_ns, attachments_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for row in rows {
        row.validate()?;
        let attachments = serde_json::to_string(&row.attachments)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        statement.execute(params![
            row.id,
            row.root_run_id,
            row.target_run_id,
            row.kind,
            row.text,
            row.in_reply_to_question_id,
            row.in_reply_to_title,
            row.reference_kind,
            row.reference_context,
            row.created_at_ns,
            attachments,
        ])?;
    }
    Ok(())
}

pub fn for_target(conn: &Connection, run_id: &str) -> Result<Vec<ReviewEvidenceRow>, StorageError> {
    list(conn, "target_run_id = ?1", run_id)
}

pub fn for_root(conn: &Connection, run_id: &str) -> Result<Vec<ReviewEvidenceRow>, StorageError> {
    list(conn, "root_run_id = ?1", run_id)
}

pub fn for_lineage(
    conn: &Connection,
    run_ids: &[String],
) -> Result<Vec<ReviewEvidenceRow>, StorageError> {
    // A single bound JSON array avoids SQLite's parameter limit without omitting targets.
    let ids = serde_json::to_string(run_ids)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    list(
        conn,
        "target_run_id IN (SELECT value FROM json_each(?1))",
        &ids,
    )
}

/// The evidence of every submission that attached skills, one entry per
/// acceptance in acceptance order (`acceptance_seq`).
pub fn attachment_acceptances(
    conn: &Connection,
    run_ids: &[String],
) -> Result<Vec<ReviewEvidenceRow>, StorageError> {
    let ids = serde_json::to_string(run_ids)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let columns = COLUMNS
        .split(", ")
        .map(|column| format!("evidence.{}", column.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut statement = conn.prepare(&format!(
        "SELECT {columns} FROM host_input_receipts AS receipt \
         JOIN review_evidence AS evidence ON evidence.id = receipt.evidence_id \
         WHERE evidence.target_run_id IN (SELECT value FROM json_each(?1)) \
           AND evidence.attachments_json <> '[]' \
         ORDER BY receipt.acceptance_seq"
    ))?;
    let rows = statement.query_map([ids], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn list(
    conn: &Connection,
    predicate: &str,
    value: &str,
) -> Result<Vec<ReviewEvidenceRow>, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM review_evidence WHERE {predicate} ORDER BY created_at_ns, id"
    ))?;
    let rows = statement.query_map([value], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<ReviewEvidenceRow> {
    let attachments: String = row.get(10)?;
    let attachments = serde_json::from_str(&attachments).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(ReviewEvidenceRow {
        id: row.get(0)?,
        root_run_id: row.get(1)?,
        target_run_id: row.get(2)?,
        kind: row.get(3)?,
        text: row.get(4)?,
        in_reply_to_question_id: row.get(5)?,
        in_reply_to_title: row.get(6)?,
        reference_kind: row.get(7)?,
        reference_context: row.get(8)?,
        attachments,
        created_at_ns: row.get(9)?,
    })
}
