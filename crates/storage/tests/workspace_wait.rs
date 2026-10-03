use event_bus::{Event, EventKind, LifecycleEvent, WorkspaceLockHolder, WorkspaceWait};
use storage::{Database, Storage, StorageConfig};

#[test]
fn wait_commands_are_redacted_in_sqlite_without_changing_live_observation() {
    let temp = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: temp.path().join("wait.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
    let event = Event::new(LifecycleEvent::WorkspaceWaitChanged {
        run_id: "run-waiting".into(),
        call_id: "call-waiting".into(),
        waiting: Some(WorkspaceWait {
            workspace_root: "/workspace".into(),
            tool_name: "shell".into(),
            command: Some(format!("GH_TOKEN={secret} gh api user")),
            holder: Some(WorkspaceLockHolder {
                run_id: "run-holder".into(),
                call_id: "call-holder".into(),
                tool_name: "shell".into(),
                command: Some(format!("GH_TOKEN={secret} gh pr checks --watch")),
            }),
        }),
    });
    storage.handle().append_event(None, &event).unwrap();
    storage.close();
    let database = Database::open(&config).unwrap();
    let persisted = database.events_all_ordered().unwrap();
    assert_eq!(persisted.len(), 1);
    let encoded = serde_json::to_string(&persisted[0].event.kind).unwrap();
    assert!(!encoded.contains(secret));
    assert!(serde_json::to_string(&event.kind).unwrap().contains(secret));
    let EventKind::Lifecycle(LifecycleEvent::WorkspaceWaitChanged {
        run_id,
        call_id,
        waiting: Some(wait),
    }) = &persisted[0].event.kind
    else {
        panic!("wait observation was lost");
    };
    assert_eq!(
        (run_id.as_str(), call_id.as_str()),
        ("run-waiting", "call-waiting")
    );
    assert_eq!(wait.holder.as_ref().unwrap().run_id, "run-holder");
    assert_eq!(wait.workspace_root, std::path::Path::new("/workspace"));
    assert!(database.restore_sessions().unwrap().is_empty());
}
