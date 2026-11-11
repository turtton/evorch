//! Durable execution intent; the payload never grants execution authority.
use crate::{Database, StorageError};
use rusqlite::{OptionalExtension, params};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeIntentRecord {
    pub run_id: String,
    pub incarnation: String,
    pub payload: String,
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
    let mut value: serde_json::Value = serde_json::from_str(&record.payload)
        .map_err(|e| StorageError::Serialization(e.to_string()))?;
    // Observational snapshots may redact; an exact replay prefix cannot.
    if secret_guard::SecretRedactor::from_env().redact_json(&mut value) != 0 {
        return Err(StorageError::Serialization(
            "resume intent contains credentials; exact replay cannot be persisted".into(),
        ));
    }
    let changed = if let Some(expected) = expected {
        conn.execute("UPDATE resume_intents SET incarnation=?2, payload=?3 WHERE run_id=?1 AND incarnation=?4",
            params![record.run_id, record.incarnation, record.payload, expected])?
    } else {
        conn.execute("INSERT INTO resume_intents(run_id, incarnation, payload) VALUES (?1, ?2, ?3) ON CONFLICT(run_id) DO NOTHING",
            params![record.run_id, record.incarnation, record.payload])?
    };
    if changed != 1 {
        return Err(StorageError::Io("stale resume intent incarnation".into()));
    }
    Ok(())
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
    fn tombstones_hide_and_fence_intents_without_resurrecting_a_deleted_run() {
        let db = Database::open_in_memory().unwrap();
        save(&db.conn, &record("old", "{}"), None).unwrap();
        db.conn.execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('run-41','thread','family')", []).unwrap();
        assert!(db.resume_intents().unwrap().is_empty());
        assert!(db.resume_intent("run-41").unwrap().is_none());
        assert!(save(&db.conn, &record("new", "{}"), Some("old")).is_err());
    }
}
