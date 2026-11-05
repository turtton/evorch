use rusqlite::{Connection, params};

use crate::StorageError;
use crate::host_input::{HostChatSubmission, HostChatSubmissionState};

/// Record a submission unless its id already has a row: the first record stays.
/// Returns whether this call stored it.
pub(crate) fn insert(
    conn: &Connection,
    submission: &HostChatSubmission,
) -> Result<bool, StorageError> {
    submission.validate()?;
    let changed = conn.execute(
        "INSERT OR IGNORE INTO host_chat_submissions \
         (submission_id, fingerprint, run_id, provenance_json, accepted_at_ns) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            submission.submission_id,
            submission.fingerprint,
            submission.run_id,
            submission.provenance_json,
            submission.accepted_at_ns,
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
    let mut statement = conn.prepare(
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
        conn.prepare("SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)")?;
    let mut deleted = Vec::new();
    for id in ids {
        if statement.query_row(params![id], |row| row.get::<_, bool>(0))? {
            deleted.push(id.clone());
        }
    }
    Ok(deleted)
}
