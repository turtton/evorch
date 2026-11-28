use rusqlite::{Connection, params};

use crate::StorageError;
use crate::host_input::{
    HOST_INPUT_EDIT_TEXT_BYTES, HostChatSubmission, HostChatSubmissionEdit,
    HostChatSubmissionState, HostInputDraft, HostInputPreview, HostInputStatus,
};

/// Record a submission unless its id already has a row: the first record stays.
/// Returns whether this call stored it.
///
/// `edit` is stored in the same row. The column holding the words comes last, so a
/// preview is read without touching the pages of a large body.
pub(crate) fn insert(
    conn: &Connection,
    submission: &HostChatSubmission,
    edit: Option<&HostChatSubmissionEdit>,
) -> Result<bool, StorageError> {
    submission.validate()?;
    if let Some(edit) = edit {
        edit.validate()?;
    }
    let changed = conn.execute(
        "INSERT OR IGNORE INTO host_chat_submissions \
         (submission_id, fingerprint, run_id, provenance_json, accepted_at_ns, \
          edit_preview, edit_preview_truncated, edit_image_count, edit_attachment_count, \
          edit_original_text) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            submission.submission_id,
            submission.fingerprint,
            submission.run_id,
            submission.provenance_json,
            submission.accepted_at_ns,
            edit.map(|e| e.preview.as_str()),
            edit.map(|e| e.preview_truncated),
            edit.map(|e| e.image_count),
            edit.map(|e| e.attachment_count),
            edit.and_then(|e| e.original_text.as_deref()),
        ],
    )?;
    Ok(changed == 1)
}

/// The submissions among `ids`, in the order asked. One whose run is tombstoned is
/// reported as deleted, without its content.
pub(crate) fn for_ids(
    conn: &Connection,
    ids: &[String],
) -> Result<Vec<HostChatSubmissionState>, StorageError> {
    let mut statement = conn.prepare_cached(
        "SELECT submission_id, fingerprint, run_id, provenance_json, accepted_at_ns, \
                run_id IN (SELECT run_id FROM run_tombstones) \
         FROM host_chat_submissions WHERE submission_id = ?1",
    )?;
    let mut found = Vec::new();
    for id in ids {
        let mut rows = statement.query_map(params![id], |row| {
            let deleted: bool = row.get(5)?;
            Ok(if deleted {
                HostChatSubmissionState::Deleted(row.get(0)?)
            } else {
                HostChatSubmissionState::Live(HostChatSubmission {
                    submission_id: row.get(0)?,
                    fingerprint: row.get(1)?,
                    run_id: row.get(2)?,
                    provenance_json: row.get(3)?,
                    accepted_at_ns: row.get(4)?,
                })
            })
        })?;
        if let Some(row) = rows.next() {
            found.push(row?);
        }
    }
    Ok(found)
}

/// The runs among `ids` that were deleted: their tombstone stays after the rows are
/// reclaimed, so this still answers once nothing else is left.
pub(crate) fn tombstoned_runs(
    conn: &Connection,
    ids: &[String],
) -> Result<Vec<String>, StorageError> {
    let mut statement =
        conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)")?;
    let mut deleted = Vec::new();
    for id in ids {
        if statement.query_row(params![id], |row| row.get::<_, bool>(0))? {
            deleted.push(id.clone());
        }
    }
    Ok(deleted)
}

/// Previews of the submissions among `ids` that have edit data and whose run is not
/// deleted. Selects the preview columns only: never the words, the receipt or provenance.
pub(crate) fn previews(
    conn: &Connection,
    ids: &[String],
) -> Result<Vec<HostInputPreview>, StorageError> {
    let mut statement = conn.prepare_cached(
        "SELECT edit_preview, edit_preview_truncated, edit_image_count, edit_attachment_count \
         FROM host_chat_submissions \
         WHERE submission_id = ?1 AND edit_preview IS NOT NULL \
           AND run_id NOT IN (SELECT run_id FROM run_tombstones)",
    )?;
    let mut found = Vec::new();
    for id in ids {
        let mut rows = statement.query_map(params![id], |row| {
            Ok(HostInputPreview {
                submission_id: id.clone(),
                preview: row.get(0)?,
                preview_truncated: row.get(1)?,
                image_count: row.get(2)?,
                attachment_count: row.get(3)?,
            })
        })?;
        if let Some(row) = rows.next() {
            found.push(row?);
        }
    }
    Ok(found)
}

/// The words of one human submission for editing. `None` when there is no such
/// submission, its run or root run is deleted, it has no edit data, or its words were
/// not kept (attachments without `edit_original_text`). Without attachments the
/// receipt's text is the words. Words above the bound are reported by size alone.
pub(crate) fn draft(
    conn: &Connection,
    submission_id: &str,
) -> Result<Option<HostInputDraft>, StorageError> {
    let mut statement = conn.prepare_cached(
        "SELECT run_id, status, image_count, attachment_count, bytes, \
                CASE WHEN bytes <= ?2 THEN body END \
         FROM (SELECT receipt.run_id AS run_id, receipt.status AS status, \
                      submission.edit_image_count AS image_count, \
                      submission.edit_attachment_count AS attachment_count, \
                      length(CAST(COALESCE(submission.edit_original_text, receipt.text) AS BLOB)) \
                          AS bytes, \
                      COALESCE(submission.edit_original_text, receipt.text) AS body \
               FROM host_chat_submissions AS submission \
               JOIN host_input_receipts AS receipt ON receipt.prompt_id = submission.submission_id \
               WHERE submission.submission_id = ?1 \
                 AND submission.edit_preview IS NOT NULL \
                 AND (submission.edit_original_text IS NOT NULL \
                      OR submission.edit_attachment_count = 0) \
                 AND json_extract(receipt.provenance_json, '$.kind') = 'user' \
                 AND receipt.run_id NOT IN (SELECT run_id FROM run_tombstones) \
                 AND receipt.root_run_id NOT IN (SELECT run_id FROM run_tombstones))",
    )?;
    let mut rows = statement.query_map(
        params![submission_id, HOST_INPUT_EDIT_TEXT_BYTES as i64],
        |row| {
            let status: String = row.get(1)?;
            Ok(HostInputDraft {
                run_id: row.get(0)?,
                status: HostInputStatus::parse(&status).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        format!("unknown host input status {status}").into(),
                    )
                })?,
                image_count: row.get(2)?,
                attachment_count: row.get(3)?,
                text_bytes: row.get::<_, i64>(4)?.max(0) as u64,
                text: row.get(5)?,
            })
        },
    )?;
    rows.next().transpose().map_err(Into::into)
}
