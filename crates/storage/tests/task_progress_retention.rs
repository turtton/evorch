use event_bus::{Event, OrchestratorEvent};
use storage::{Database, Storage, StorageConfig};

#[test]
fn normal_append_without_reconcile_is_retained_using_bounded_event_replay() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    for (status, input) in [
        ("running", Some("Original title")),
        ("running", None),
        ("completed", None),
    ] {
        storage
            .handle()
            .append_event(
                None,
                &Event::new(OrchestratorEvent::TaskProgressed {
                    task_id: "task".into(),
                    run_id: "run".into(),
                    progress: serde_json::json!({
                        "status": status, "input": input, "attempts": 1
                    }),
                    reason: "boundary".into(),
                }),
            )
            .unwrap();
    }
    assert!(
        Database::open(&config)
            .unwrap()
            .task("task")
            .unwrap()
            .is_none()
    );
    storage.handle().checkpoint_now().unwrap();
    storage.close();
    let db = Database::open(&config).unwrap();
    assert!(db.task("task").unwrap().is_none());
    let retained = db.orchestrator_events_ordered().unwrap();
    assert_eq!(retained.len(), 2);
    let event_bus::EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { progress, .. }) =
        &retained[0].event.kind
    else {
        panic!("first input event was not retained")
    };
    assert_eq!(progress["input"], "Original title");
}
