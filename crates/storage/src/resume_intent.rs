//! Durable execution intent; the payload never grants execution authority.
use crate::{Database, StorageError};
use rusqlite::{OptionalExtension, params};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeIntentRecord {
    pub run_id: String,
    pub incarnation: String,
    pub payload: String,
}

/// Acknowledged conditional write for a completed or fenced, unregistered intent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum ResumeIntentWriteOutcome {
    /// The still-current incarnation was updated.
    Saved,
    /// A durable successor already exists and was left unchanged.
    Replaced { current: String },
}

/// A human recovery Continue replaces exactly the released/activated revision.
#[derive(Debug, Clone)]
pub struct RecoveryContinueFence {
    pub record: ResumeIntentRecord,
    pub expected_incarnation: String,
    pub expected_revision: u64,
}

impl Database {
    pub fn resume_intent(&self, run_id: &str) -> Result<Option<ResumeIntentRecord>, StorageError> {
        Ok(self.conn.query_row("SELECT run_id, incarnation, payload FROM resume_intents WHERE run_id=?1 AND run_id NOT IN (SELECT run_id FROM run_tombstones)", [run_id], |row| Ok(ResumeIntentRecord { run_id: row.get(0)?, incarnation: row.get(1)?, payload: row.get(2)? })).optional()?)
    }
    pub fn resume_intents(&self) -> Result<Vec<ResumeIntentRecord>, StorageError> {
        let mut query = self.conn.prepare("SELECT run_id, incarnation, payload FROM resume_intents WHERE run_id NOT IN (SELECT run_id FROM run_tombstones) ORDER BY run_id")?;
        Ok(query
            .query_map([], |row| {
                Ok(ResumeIntentRecord {
                    run_id: row.get(0)?,
                    incarnation: row.get(1)?,
                    payload: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }
}

pub(crate) fn save(
    conn: &rusqlite::Connection,
    record: &ResumeIntentRecord,
    expected: Option<&str>,
) -> Result<(), StorageError> {
    save_with_revision(conn, record, expected, None)
}

pub(crate) fn save_with_revision(
    conn: &rusqlite::Connection,
    record: &ResumeIntentRecord,
    expected: Option<&str>,
    revision: Option<u64>,
) -> Result<(), StorageError> {
    validate_payload(record)?;
    // Only the recovery release -> manual admission path adds a revision fence.
    // Ordinary saves, retirement, and unregistered stops retain their own CAS.
    let changed = if let (Some(expected), Some(revision)) = (expected, revision) {
        conn.execute(
            "UPDATE resume_intents SET incarnation=?2, payload=?3 WHERE run_id=?1 AND incarnation=?4 AND revision=?5",
            params![record.run_id, record.incarnation, record.payload, expected, revision],
        )?
    } else {
        write(conn, record, expected)?
    };
    if changed != 1 {
        return Err(StorageError::Io("stale resume intent incarnation".into()));
    }
    Ok(())
}

fn validate_payload(record: &ResumeIntentRecord) -> Result<serde_json::Value, StorageError> {
    let value: serde_json::Value = serde_json::from_str(&record.payload)
        .map_err(|e| StorageError::Serialization(e.to_string()))?;
    // Observational snapshots may redact; an exact replay prefix cannot.
    crate::replay_secrets::validate_intent(&value, &secret_guard::SecretRedactor::from_env())?;
    Ok(value)
}

fn write(
    conn: &rusqlite::Connection,
    record: &ResumeIntentRecord,
    expected: Option<&str>,
) -> Result<usize, StorageError> {
    Ok(if let Some(expected) = expected {
        conn.execute("UPDATE resume_intents SET incarnation=?2, payload=?3 WHERE run_id=?1 AND incarnation=?4",
            params![record.run_id, record.incarnation, record.payload, expected])?
    } else {
        conn.execute("INSERT INTO resume_intents(run_id, incarnation, payload) VALUES (?1, ?2, ?3) ON CONFLICT(run_id) DO NOTHING",
            params![record.run_id, record.incarnation, record.payload])?
    })
}

pub(crate) fn retire(
    conn: &rusqlite::Connection,
    record: &ResumeIntentRecord,
) -> Result<ResumeIntentWriteOutcome, StorageError> {
    if validate_payload(record)?
        .get("active")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return Err(StorageError::Serialization(
            "retirement requires an inactive resume intent".into(),
        ));
    }
    write_current_or_replaced(conn, record)
}

pub(crate) fn stop_unregistered(
    conn: &rusqlite::Connection,
    record: &ResumeIntentRecord,
) -> Result<ResumeIntentWriteOutcome, StorageError> {
    let value = validate_payload(record)?;
    if value
        .get("resume_requested")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
        || !matches!(
            value.get("stop_reason").and_then(serde_json::Value::as_str),
            Some("operator_stopped" | "cancelled" | "cli_disconnected")
        )
    {
        return Err(StorageError::Serialization(
            "unregistered stop requires a non-resuming stopped intent".into(),
        ));
    }
    write_current_or_replaced(conn, record)
}

fn write_current_or_replaced(
    conn: &rusqlite::Connection,
    record: &ResumeIntentRecord,
) -> Result<ResumeIntentWriteOutcome, StorageError> {
    let tx = conn.unchecked_transaction()?;
    let outcome = if write(&tx, record, Some(&record.incarnation))? == 1 {
        ResumeIntentWriteOutcome::Saved
    } else {
        // The writer transaction distinguishes a real successor from missing or
        // tombstoned rows without a check/write race. I/O errors remain errors.
        let current: Option<String> = tx
            .query_row(
                "SELECT incarnation FROM resume_intents WHERE run_id=?1
                 AND run_id NOT IN (SELECT run_id FROM run_tombstones)",
                [&record.run_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current.filter(|incarnation| *incarnation != record.incarnation) else {
            return Err(StorageError::Io("stale resume intent incarnation".into()));
        };
        ResumeIntentWriteOutcome::Replaced { current }
    };
    tx.commit()?;
    Ok(outcome)
}

const RELEASABLE_ROW: &str = "json_valid(payload)
      AND json_extract(payload, '$.active') = 0
      AND json_array_length(payload, '$.provider_input.request.messages') > 0";
const UNFENCED: &str = "run_id NOT IN (SELECT run_id FROM run_tombstones)
      AND run_id NOT IN (
        SELECT m.run_id FROM recovery_members m JOIN recovery_manifests f USING(manifest_id)
        WHERE f.superseded = 0 AND (
          f.committed = 0
          OR EXISTS(SELECT 1 FROM recovery_claims c
                    WHERE c.manifest_id = f.manifest_id AND c.state IN ('prepared', 'activated'))
          OR EXISTS(SELECT 1 FROM recovery_members r JOIN resume_intents i ON i.run_id = r.run_id
                    WHERE r.manifest_id = f.manifest_id AND r.resume = 1
                      AND json_extract(i.payload, '$.active') = 1
                      AND json_type(i.payload, '$.stop_reason') = 'null'))
        UNION
        SELECT e.run_id FROM restart_expected_members e JOIN restart_transactions t USING(restart_id)
        WHERE t.phase != 'completed' AND (
          t.phase NOT IN ('aborted', 'invalidated', 'superseded')
          OR EXISTS(SELECT 1 FROM restart_components c
                    WHERE c.restart_id = t.restart_id AND c.disposition = 'retry_pending')))";
const RELEASE_TABLES: [&str; 8] = [
    "resume_intents",
    "run_tombstones",
    "recovery_manifests",
    "recovery_members",
    "recovery_claims",
    "restart_transactions",
    "restart_expected_members",
    "restart_components",
];
const RELEASE_BATCH_ROWS: usize = 64;

fn release_supported(conn: &rusqlite::Connection) -> Result<bool, StorageError> {
    let present: usize = conn.query_row(
        &format!(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ({})",
            RELEASE_TABLES.map(|name| format!("'{name}'")).join(",")
        ),
        [],
        |row| row.get(0),
    )?;
    Ok(present == RELEASE_TABLES.len())
}

/// Inactive intents saved before the runtime started releasing their provider
/// messages. The rewrite advances each revision, so a row that a manifest could
/// still claim or an unfinished restart still expects is left alone, and callers
/// run it only before any recovery reads revisions: writer startup and offline
/// compaction. Databases older than the recovery tables have nothing to release.
pub(crate) fn inactive_provider_message_rows(
    conn: &rusqlite::Connection,
) -> Result<Vec<String>, StorageError> {
    if !release_supported(conn)? {
        return Ok(Vec::new());
    }
    let mut query = conn.prepare(&format!(
        "SELECT run_id FROM resume_intents WHERE {RELEASABLE_ROW} AND {UNFENCED}"
    ))?;
    Ok(query
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?)
}

pub(crate) fn release_inactive_provider_messages(
    conn: &rusqlite::Connection,
) -> Result<u64, StorageError> {
    let candidates = inactive_provider_message_rows(conn)?;
    let mut released = 0;
    for batch in candidates.chunks(RELEASE_BATCH_ROWS) {
        let tx = conn.unchecked_transaction()?;
        {
            let mut update = tx.prepare(&format!(
                "UPDATE resume_intents
                 SET payload = json_set(payload, '$.provider_input.request.messages', json('[]'))
                 WHERE run_id = ?1 AND {RELEASABLE_ROW}"
            ))?;
            for run_id in batch {
                released += update.execute([run_id])? as u64;
            }
        }
        tx.commit()?;
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(incarnation: &str, payload: &str) -> ResumeIntentRecord {
        ResumeIntentRecord {
            run_id: "run-41".into(),
            incarnation: incarnation.into(),
            payload: payload.into(),
        }
    }
    #[test]
    fn admitted_ids_survive_before_any_context_and_stale_incarnations_cannot_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::StorageConfig {
            db_path: dir.path().join("intents.db"),
            ..Default::default()
        };
        let storage = crate::Storage::open(config.clone()).unwrap();
        let handle = storage.handle();
        handle
            .save_resume_intent(record("old", "{\"inputs\":[\"queued\"]}"), None)
            .unwrap();
        handle.flush_resume_intents().unwrap();
        let db = Database::open(&config).unwrap();
        assert!(db.persisted_run_ids().unwrap().contains(&"run-41".into()));
        assert!(
            handle
                .save_resume_intent(record("duplicate", "{}"), None)
                .is_err()
        );
        handle
            .save_resume_intent(record("new", "{\"inputs\":[\"kept\"]}"), Some("old".into()))
            .unwrap();
        assert!(
            handle
                .save_resume_intent(record("old", "{}"), Some("old".into()))
                .is_err()
        );
        assert_eq!(
            db.resume_intent("run-41").unwrap().unwrap().incarnation,
            "new"
        );
        storage.close();
        assert_eq!(
            Database::open(&config).unwrap().resume_intents().unwrap()[0].payload,
            "{\"inputs\":[\"kept\"]}"
        );
        assert!(handle.flush_resume_intents().is_err());
    }
    #[test]
    fn retirement_distinguishes_replacement_without_weakening_other_writes() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::StorageConfig {
            db_path: dir.path().join("retirement.db"),
            ..Default::default()
        };
        let storage = crate::Storage::open(config.clone()).unwrap();
        let handle = storage.handle();
        let active = record("old", r#"{"active":true,"wire":["sent"]}"#);
        let retired = record("old", r#"{"active":false,"wire":["sent"]}"#);
        handle.save_resume_intent(active.clone(), None).unwrap();
        assert!(handle.retire_resume_intent(active.clone()).is_err());
        assert!(handle.stop_unregistered_resume_intent(active).is_err());
        let stopped = record(
            "old",
            r#"{"active":true,"resume_requested":false,"stop_reason":"cancelled","wire":["sent"]}"#,
        );
        assert_eq!(
            handle
                .stop_unregistered_resume_intent(stopped.clone())
                .unwrap(),
            ResumeIntentWriteOutcome::Saved
        );
        assert_eq!(
            handle.retire_resume_intent(retired.clone()).unwrap(),
            ResumeIntentWriteOutcome::Saved
        );
        let successor = record("new", r#"{"active":true,"wire":["sent","successor"]}"#);
        handle
            .save_resume_intent(successor.clone(), Some("old".into()))
            .unwrap();
        assert_eq!(
            handle.retire_resume_intent(retired.clone()).unwrap(),
            ResumeIntentWriteOutcome::Replaced {
                current: "new".into()
            }
        );
        assert!(
            handle
                .save_resume_intent(retired.clone(), Some("old".into()))
                .is_err()
        );
        assert_eq!(
            handle
                .stop_unregistered_resume_intent(stopped.clone())
                .unwrap(),
            ResumeIntentWriteOutcome::Replaced {
                current: "new".into()
            }
        );
        let missing = ResumeIntentRecord {
            run_id: "missing".into(),
            ..retired.clone()
        };
        assert!(handle.retire_resume_intent(missing.clone()).is_err());
        assert!(
            handle
                .stop_unregistered_resume_intent(ResumeIntentRecord {
                    run_id: missing.run_id,
                    ..stopped.clone()
                })
                .is_err()
        );
        handle.flush_resume_intents().unwrap();
        let db = Database::open(&config).unwrap();
        assert_eq!(db.resume_intent("run-41").unwrap(), Some(successor));
        db.conn.execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('run-41','thread','family')", []).unwrap();
        assert!(handle.retire_resume_intent(retired).is_err());
        assert!(handle.stop_unregistered_resume_intent(stopped).is_err());
    }

    #[test]
    fn tombstones_hide_and_fence_intents_without_resurrecting_a_deleted_run() {
        let db = Database::open_in_memory().unwrap();
        save(&db.conn, &record("old", "{}"), None).unwrap();
        db.conn.execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('run-41','thread','family')", []).unwrap();
        assert!(db.resume_intents().unwrap().is_empty());
        assert!(db.resume_intent("run-41").unwrap().is_none());
        assert!(save(&db.conn, &record("new", "{}"), Some("old")).is_err());
    }
    #[test]
    fn writer_startup_releases_messages_of_inactive_intents_outside_recovery_fences() {
        // Given: inactive, active and manifest-fenced intents that all carry messages
        let dir = tempfile::tempdir().unwrap();
        let config = crate::StorageConfig {
            db_path: dir.path().join("intents.db"),
            ..Default::default()
        };
        let intent = |run: &str, active: bool| {
            serde_json::json!({
                "run_id": run, "incarnation": "i", "active": active,
                "provider_input": {"edit_tool": "edit", "request": {
                    "model": "m", "messages": [{"role": "user", "content": []}],
                    "tools": [{"name": "read"}]
                }}
            })
            .to_string()
        };
        {
            let db = Database::open(&config).unwrap();
            for (run, active) in [
                ("run-done", false),
                ("run-live", true),
                ("run-fenced", false),
                ("run-dead-fence", false),
            ] {
                db.conn
                    .execute(
                        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'i',?2)",
                        params![run, intent(run, active)],
                    )
                    .unwrap();
            }
            db.conn
                .execute_batch(
                    "INSERT INTO recovery_manifests(manifest_id,schema_version,member_count,edge_count,edges_json)
                     VALUES('m',1,1,0,'[]');
                     INSERT INTO recovery_members(manifest_id,run_id,incarnation,intent_revision,resume,record_json)
                     VALUES('m','run-fenced','i',0,0,'{}');
                     INSERT INTO recovery_manifests(manifest_id,schema_version,committed,member_count,edge_count,edges_json)
                     VALUES('finished',1,1,1,0,'[]');
                     INSERT INTO recovery_members(manifest_id,run_id,incarnation,intent_revision,resume,record_json)
                     VALUES('finished','run-dead-fence','i',0,1,'{}');",
                )
                .unwrap();
        }

        // When: the writer opens the database
        drop(crate::Storage::open(config.clone()).unwrap());

        // Then: inactive intents lost their messages unless a manifest can still claim them
        let db = Database::open(&config).unwrap();
        let read = |run: &str| -> (serde_json::Value, u64) {
            db.conn
                .query_row(
                    "SELECT payload,revision FROM resume_intents WHERE run_id=?1",
                    [run],
                    |row| {
                        Ok((
                            serde_json::from_str(&row.get::<_, String>(0)?).unwrap(),
                            row.get(1)?,
                        ))
                    },
                )
                .unwrap()
        };
        let messages = |value: &serde_json::Value| {
            value["provider_input"]["request"]["messages"]
                .as_array()
                .unwrap()
                .len()
        };
        let (done, done_revision) = read("run-done");
        assert_eq!(messages(&done), 0);
        assert_eq!(
            done["provider_input"]["request"]["tools"],
            serde_json::json!([{"name": "read"}])
        );
        assert_eq!(done["provider_input"]["edit_tool"], "edit");
        assert_eq!(done_revision, 1);
        assert_eq!(
            read("run-live"),
            (serde_json::from_str(&intent("run-live", true)).unwrap(), 0)
        );
        assert_eq!(
            read("run-fenced"),
            (
                serde_json::from_str(&intent("run-fenced", false)).unwrap(),
                0
            )
        );
        assert_eq!(messages(&read("run-dead-fence").0), 0);
    }

    #[test]
    fn databases_without_recovery_tables_have_nothing_to_release() {
        // Given: a database older than the recovery tables
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE resume_intents(run_id TEXT PRIMARY KEY, incarnation TEXT, payload TEXT)",
        )
        .unwrap();

        // When / Then: the release is a no-op instead of an error
        assert!(inactive_provider_message_rows(&conn).unwrap().is_empty());
        assert_eq!(release_inactive_provider_messages(&conn).unwrap(), 0);
    }
}
