//! Run context snapshot persistence through the public storage API.

use storage::{Database, RunContextRecord, Storage, StorageConfig};
use tempfile::TempDir;

fn record() -> RunContextRecord {
    RunContextRecord {
        run_id: "run".into(),
        role: "worker".into(),
        name: "initial".into(),
        parent_run_id: Some("parent".into()),
        config_json: r#"{"model":"first"}"#.into(),
        messages_json: r#"[{"content":"hello"}]"#.into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "completed".into(),
        restorable: true,
        updated_at_ns: 10,
    }
}

#[test]
fn upsert_latest_wins_and_round_trips() {
    // Given: a stored snapshot and another run that must remain independent.
    let temp = TempDir::new().unwrap();
    let config = StorageConfig {
        db_path: temp.path().join("contexts.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let initial = record();
    handle.upsert_run_context(&initial).unwrap();
    assert_eq!(
        Database::open(&config).unwrap().run_context("run").unwrap(),
        Some(initial.clone())
    );
    let other = RunContextRecord {
        run_id: "other".into(),
        ..initial
    };
    handle.upsert_run_context(&other).unwrap();
    let latest = RunContextRecord {
        run_id: "run".into(),
        role: "reviewer".into(),
        name: "latest".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: "[]".into(),
        checkpoints_json: r#"[{"id":"checkpoint"}]"#.into(),
        terminal_phase: "failed".into(),
        restorable: false,
        updated_at_ns: 20,
    };
    // When: replacing the snapshot and reopening after writer shutdown.
    handle.upsert_run_context(&latest).unwrap();
    storage.close();
    let db = Database::open(&config).unwrap();
    // Then: every field is replaced and unrelated/missing runs stay independent.
    assert_eq!(db.run_context("run").unwrap(), Some(latest));
    assert_eq!(db.run_context("other").unwrap(), Some(other));
    assert_eq!(db.run_context("missing").unwrap(), None);
}

#[test]
fn suspended_writer_rejects_context_upsert() {
    // Given: a writer suspended by the database size limit.
    let temp = TempDir::new().unwrap();
    let mut config = StorageConfig {
        db_path: temp.path().join("contexts.db"),
        ..StorageConfig::default()
    };
    config.hard_limits.max_db_bytes = 1;
    let storage = Storage::open(config.clone()).unwrap();
    // When: attempting to persist a snapshot.
    let result = storage.handle().upsert_run_context(&record());
    // Then: the snapshot is rejected and absent.
    assert!(result.is_err());
    assert_eq!(
        Database::open(&config).unwrap().run_context("run").unwrap(),
        None
    );
}

#[test]
fn snapshot_secret_diagnostics_omit_values_and_decode_json_escapes() {
    // Given: escaped provider reasoning and checkpoint text containing a key.
    let temp = TempDir::new().unwrap();
    let config = StorageConfig {
        db_path: temp.path().join("contexts.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
    for checkpoint in [false, true] {
        let mut snapshot = record();
        let payload = format!(r#"[{{"text":"{}"}}]"#, secret.replace('s', "\\u0073"));
        if checkpoint {
            snapshot.checkpoints_json = payload;
        } else {
            snapshot.messages_json = payload;
        }
        // When: the public writer receives the snapshot.
        let error = storage.handle().upsert_run_context(&snapshot).unwrap_err();
        // Then: a typed secret rejection reveals no offending value and persists nothing.
        assert!(matches!(
            error,
            storage::StorageError::SecretDetected { .. }
        ));
        assert!(!format!("{error:?} {error}").contains(secret));
        assert!(
            Database::open(&config)
                .unwrap()
                .run_context("run")
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn snapshot_rejects_secret_object_keys_without_diagnostic_leaks() {
    // Given: direct and JSON-escaped keys in either persisted history field.
    let temp = TempDir::new().unwrap();
    let config = StorageConfig {
        db_path: temp.path().join("keys.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
    for checkpoint in [false, true] {
        for key in [secret.to_owned(), secret.replace('s', "\\u0073")] {
            let mut snapshot = record();
            let payload = format!(r#"[{{"input":{{"{key}":"private-value"}}}}]"#);
            if checkpoint {
                snapshot.checkpoints_json = payload;
            } else {
                snapshot.messages_json = payload;
            }
            // When: persisting the snapshot through the real writer.
            let result = storage.handle().upsert_run_context(&snapshot);
            // Then: rejection identifies only the field, never the key or value.
            let error = result.expect_err("secret object key must be rejected");
            assert!(
                matches!(&error, storage::StorageError::SecretDetected { field, .. }
                if *field == if checkpoint { "checkpoints_json" } else { "messages_json" })
            );
            let diagnostic = format!("{error:?} {error}");
            assert!(!diagnostic.contains(secret));
            assert!(!diagnostic.contains(&key));
            assert!(!diagnostic.contains("private-value"));
            assert!(
                Database::open(&config)
                    .unwrap()
                    .run_context("run")
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn invalidation_updates_only_restore_metadata_when_legacy_history_contains_secrets() {
    // Given: two legacy snapshots with secrets in both history payloads.
    let temp = TempDir::new().unwrap();
    let config = StorageConfig {
        db_path: temp.path().join("legacy.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
    for id in ["run", "other"] {
        conn.execute(
            "INSERT INTO run_contexts \
             (run_id, role, name, parent_run_id, config_json, messages_json, \
             checkpoints_json, terminal_phase, updated_at_ns, restorable) \
             VALUES (?1, 'Worker', 'legacy', NULL, \
             '{\"restorable\":true}', ?2, ?2, 'Done', 10, 1)",
            rusqlite::params![id, format!(r#"[{{"text":"{secret}"}}]"#)],
        )
        .unwrap();
    }
    let db = Database::open(&config).unwrap();
    let before = db.run_context("run").unwrap().unwrap();
    let other = db.run_context("other").unwrap().unwrap();
    // When: invalidating one run through the public single-writer API.
    storage.handle().invalidate_run_context("run").unwrap();
    storage.close();
    // Then: reopening preserves payloads and the unrelated run, but denies restore.
    let db = Database::open(&config).unwrap();
    let after = db.run_context("run").unwrap().unwrap();
    assert!(!after.restorable);
    assert_eq!(after.messages_json, before.messages_json);
    assert_eq!(after.checkpoints_json, before.checkpoints_json);
    assert_eq!(after.updated_at_ns, before.updated_at_ns);
    let descriptor: serde_json::Value = serde_json::from_str(&after.config_json).unwrap();
    assert_eq!(descriptor["restorable"], false);
    assert_eq!(descriptor["non_restorable_reason"], "persist_failed");
    assert_eq!(db.run_context("other").unwrap().unwrap(), other);
}
