use rusqlite::{Connection, params};

use crate::StorageError;
use crate::host_input::{HostInputReceipt, HostInputStatus, HostInputTransition, HostInputWrite};

const COLUMNS: &str = "prompt_id, run_id, root_run_id, text, image_count, status, reason, \
                       accepted_at_ns, status_at_ns, evidence_id";

/// Canonical JSON array of distinct, non-empty prompt IDs in first-seen order.
pub(crate) fn normalized_ids(json: &str) -> Result<String, StorageError> {
    let ids = parse_ids(json)?;
    serde_json::to_string(&ids).map_err(|error| StorageError::Serialization(error.to_string()))
}

pub(crate) fn parse_ids(json: &str) -> Result<Vec<String>, StorageError> {
    let ids: Vec<String> = serde_json::from_str(if json.is_empty() { "[]" } else { json })
        .map_err(|error| StorageError::Serialization(format!("host inputs: {error}")))?;
    let mut seen = std::collections::HashSet::new();
    Ok(ids
        .into_iter()
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect())
}

pub(crate) fn insert(conn: &Connection, receipt: &HostInputReceipt) -> Result<(), StorageError> {
    receipt.validate()?;
    let image_count = i64::from(receipt.image_count);
    conn.execute(
        &format!(
            "INSERT INTO host_input_receipts ({COLUMNS}, acceptance_seq) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
              (SELECT COALESCE(MAX(acceptance_seq), 0) + 1 FROM host_input_receipts))"
        ),
        params![
            receipt.prompt_id,
            receipt.run_id,
            receipt.root_run_id,
            receipt.text,
            image_count,
            receipt.status.as_str(),
            receipt.reason,
            receipt.accepted_at_ns,
            receipt.status_at_ns,
            receipt.evidence_id,
        ],
    )?;
    Ok(())
}

/// Move `Accepted` receipts to their requested final state and return the
/// transitions that changed a row. Final states never change here.
pub(crate) fn transition(
    conn: &Connection,
    transitions: &[HostInputTransition],
    now_ns: i64,
) -> Result<Vec<HostInputTransition>, StorageError> {
    let mut statement = conn.prepare(
        "UPDATE host_input_receipts SET status = ?2, reason = ?3, status_at_ns = ?4 \
         WHERE prompt_id = ?1 AND status = 'accepted'",
    )?;
    let mut applied = Vec::new();
    for transition in transitions {
        if !transition.status.is_terminal() {
            return Err(StorageError::Serialization(
                "host input receipts only move to a final state".into(),
            ));
        }
        if statement.execute(params![
            transition.prompt_id,
            transition.status.as_str(),
            transition.reason,
            now_ns
        ])? == 1
        {
            applied.push(transition.clone());
        }
    }
    Ok(applied)
}

/// Mark the receipts a restored run's earlier incarnations left behind as
/// `Unknown`: `Accepted` ones, and `Delivered` ones its snapshot does not hold.
fn reconcile(
    conn: &Connection,
    run_id: &str,
    host_inputs: &[String],
    reason: &str,
    now_ns: i64,
) -> Result<Vec<HostInputTransition>, StorageError> {
    let held = serde_json::to_string(host_inputs)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let mut statement = conn.prepare(
        "UPDATE host_input_receipts SET status = 'unknown', reason = ?3, status_at_ns = ?4 \
         WHERE run_id = ?1 AND (status = 'accepted' OR (status = 'delivered' AND prompt_id NOT IN \
         (SELECT value FROM json_each(?2)))) RETURNING prompt_id",
    )?;
    let rows = statement.query_map(params![run_id, held, reason, now_ns], |row| row.get(0))?;
    rows.map(|prompt_id| {
        Ok(HostInputTransition {
            prompt_id: prompt_id?,
            status: HostInputStatus::Unknown,
            reason: Some(reason.into()),
        })
    })
    .collect()
}

/// Apply a write alongside `record`, the snapshot committed in the same transaction.
pub(crate) fn apply(
    conn: &Connection,
    record: Option<&crate::RunContextRecord>,
    write: &HostInputWrite,
    now_ns: i64,
) -> Result<Vec<HostInputTransition>, StorageError> {
    let mut applied = Vec::new();
    if let Some(reason) = &write.reconcile_reason {
        let record = record.ok_or_else(|| {
            StorageError::Serialization("host input reconciliation requires a snapshot".into())
        })?;
        applied.extend(reconcile(
            conn,
            &record.run_id,
            &parse_ids(&record.host_inputs_json)?,
            reason,
            now_ns,
        )?);
    }
    for receipt in &write.inserts {
        insert(conn, receipt)?;
    }
    applied.extend(transition(conn, &write.transitions, now_ns)?);
    Ok(applied)
}

/// Accepted receipts of `run_id` become `Unknown`; returns the changed IDs.
pub(crate) fn mark_unknown(
    conn: &Connection,
    run_id: &str,
    reason: &str,
    now_ns: i64,
) -> Result<Vec<String>, StorageError> {
    let mut statement = conn.prepare(
        "UPDATE host_input_receipts SET status = 'unknown', reason = ?2, status_at_ns = ?3 \
         WHERE run_id = ?1 AND status = 'accepted' RETURNING prompt_id",
    )?;
    let rows = statement.query_map(params![run_id, reason, now_ns], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Receipts for the given prompt IDs, in request order; missing IDs are skipped.
pub(crate) fn for_ids(
    conn: &Connection,
    ids: &[String],
) -> Result<Vec<HostInputReceipt>, StorageError> {
    let ids = serde_json::to_string(ids)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let mut statement = conn.prepare(&format!(
        "SELECT {} FROM json_each(?1) AS requested \
         JOIN host_input_receipts ON host_input_receipts.prompt_id = requested.value \
         ORDER BY requested.key",
        COLUMNS
            .split(", ")
            .map(|column| format!("host_input_receipts.{column}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))?;
    let rows = statement.query_map([ids], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Receipts of one run, oldest first.
pub(crate) fn for_run(
    conn: &Connection,
    run_id: &str,
) -> Result<Vec<HostInputReceipt>, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM host_input_receipts WHERE run_id = ?1 \
         ORDER BY accepted_at_ns, prompt_id"
    ))?;
    let rows = statement.query_map([run_id], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostInputReceipt> {
    let status: String = row.get(5)?;
    let image_count: i64 = row.get(4)?;
    Ok(HostInputReceipt {
        prompt_id: row.get(0)?,
        run_id: row.get(1)?,
        root_run_id: row.get(2)?,
        text: row.get(3)?,
        image_count: u32::try_from(image_count).unwrap_or(u32::MAX),
        status: HostInputStatus::parse(&status).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                format!("unknown host input status {status}").into(),
            )
        })?,
        reason: row.get(6)?,
        accepted_at_ns: row.get(7)?,
        status_at_ns: row.get(8)?,
        evidence_id: row.get(9)?,
    })
}

/// What each given submission recorded, in request order; IDs without a
/// receipt are skipped. A submission without evidence has no skills.
///
/// Only the skill names are projected out of the stored attachments, in SQL, so
/// no skill body is read into memory. A row whose evidence cannot be read
/// reports that on its own entry (`unreadable`) and never fails the others.
pub(crate) fn origins(
    conn: &Connection,
    ids: &[String],
) -> Result<Vec<crate::HostInputOrigin>, StorageError> {
    let ids = serde_json::to_string(ids)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let mut statement = conn.prepare(
        "SELECT receipt.prompt_id, receipt.text, receipt.evidence_id, evidence.id, \
                CASE WHEN evidence.id IS NULL THEN NULL \
                     WHEN NOT json_valid(evidence.attachments_json) THEN NULL \
                     WHEN json_type(evidence.attachments_json) <> 'array' THEN NULL \
                     WHEN EXISTS (SELECT 1 FROM json_each(evidence.attachments_json) AS item \
                                  WHERE CASE WHEN item.type <> 'object' THEN 1 \
                                             WHEN json_type(item.value, '$.name') IS NOT 'text' \
                                             THEN 1 ELSE 0 END = 1) THEN NULL \
                     ELSE (SELECT json_group_array(json_extract(attachment.value, '$.name') \
                                                   ORDER BY attachment.key) \
                           FROM json_each(evidence.attachments_json) AS attachment) END \
         FROM json_each(?1) AS requested \
         JOIN host_input_receipts AS receipt ON receipt.prompt_id = requested.value \
         LEFT JOIN review_evidence AS evidence ON evidence.id = receipt.evidence_id \
         ORDER BY requested.key",
    )?;
    let rows = statement.query_map([ids], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut origins = Vec::new();
    for row in rows {
        let (prompt_id, text, evidence_id, evidence_found, names) = row?;
        let (skills, unreadable) = match (evidence_id, evidence_found, names) {
            (None, ..) => (Vec::new(), None),
            (Some(_), None, _) => (Vec::new(), Some("its evidence row is missing".to_owned())),
            (Some(_), Some(_), None) => (
                Vec::new(),
                Some("its attachments are not a list of named skills".to_owned()),
            ),
            (Some(_), Some(_), Some(names)) => match serde_json::from_str::<Vec<String>>(&names) {
                Ok(names) => (names, None),
                Err(error) => (
                    Vec::new(),
                    Some(format!("its attachments have no readable names: {error}")),
                ),
            },
        };
        origins.push(crate::HostInputOrigin {
            prompt_id,
            text,
            skills,
            unreadable,
        });
    }
    Ok(origins)
}
