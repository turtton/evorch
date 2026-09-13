use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};
use storage::{Database, Storage, StorageConfig, StorageError};

#[test]
fn diagnostic_secret_is_rejected_before_append() {
    // Given: diagnostic free text containing a credential-shaped value.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = StorageConfig {
        db_path: dir.path().join("ledger.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage");
    let event = Event::new(DiagnosticEvent {
        source: "lsp".into(),
        severity: DiagnosticSeverity::Error,
        code: "publish_diagnostics".into(),
        detail: serde_json::json!({"file":"/workspace/file.rs","count":1,"codes":["AKIAIOSFODNN7EXAMPLE"]}).to_string(),
        run_id: Some("run-lsp".into()),
        thread_id: None,
        call_id: None,
    });
    // When: appending through the public writer.
    let result = storage.handle().append_event(None, &event);
    // Then: the existing secret boundary rejects it and leaves the ledger empty.
    assert!(
        matches!(result, Err(StorageError::SecretDetected { .. })),
        "{result:?}"
    );
    assert!(
        Database::open(&config)
            .expect("db")
            .events_all_ordered()
            .expect("read")
            .is_empty()
    );
}

#[test]
fn diagnostic_call_id_survives_append_and_replay() {
    // Given: a diagnostic event correlated to a tool call.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = StorageConfig {
        db_path: dir.path().join("ledger.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage");
    let event = Event::new(DiagnosticEvent {
        source: "lsp".into(),
        severity: DiagnosticSeverity::Warning,
        code: "publish_diagnostics".into(),
        detail: serde_json::json!({"file":"/workspace/file.rs","count":1,"codes":["E0308"]})
            .to_string(),
        run_id: Some("run-1".into()),
        thread_id: Some("thread-1".into()),
        call_id: Some("call-42".into()),
    });
    // When: appending and replaying the event through storage.
    storage
        .handle()
        .append_event(Some("session-1"), &event)
        .expect("append");
    drop(storage);
    let replayed = Database::open(&config)
        .expect("db")
        .events_all_ordered()
        .expect("replay");
    // Then: call correlation remains intact.
    assert_eq!(replayed[0].event, event);
}
