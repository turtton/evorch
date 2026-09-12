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
