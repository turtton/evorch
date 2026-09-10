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
        source: "process_owner".into(),
        severity: DiagnosticSeverity::Error,
        code: "claim_conflict".into(),
        detail: "AKIAIOSFODNN7EXAMPLE".into(),
        run_id: None,
        thread_id: None,
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
