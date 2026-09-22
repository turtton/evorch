use event_bus::{Event, LifecycleEvent, OrchestratorEvent};
use std::time::UNIX_EPOCH;
use storage::{
    Database, Storage, StorageConfig,
    entity::{TaskRecord, TaskStatus},
};

#[test]
fn work_list_excludes_execution_projections_and_keeps_explicit_tasks_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("tasks.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    // A run-shaped ID is still a real task when explicitly queued; spelling is not provenance.
    handle
        .enqueue_task(&TaskRecord {
            id: "run-99".into(),
            session_id: None,
            status: TaskStatus::Pending,
            parent_run_id: None,
            input: None,
            progress: None,
            last_artifact: None,
            failure_reason: None,
            resume_cursor: None,
            attempts: 0,
            heartbeat_at: None,
            created_at: UNIX_EPOCH,
            updated_at: UNIX_EPOCH,
        })
        .unwrap();
    // Include more execution rows than the legacy queued_tasks() limit.
    for id in 1..=110 {
        handle
            .append_event(
                None,
                &Event::new(LifecycleEvent::BackgroundTaskStarted {
                    task_id: format!("execution-{id}"),
                }),
            )
            .unwrap();
    }
    handle
        .append_event(
            None,
            &Event::new(OrchestratorEvent::TaskProgressed {
                task_id: "work-item".into(),
                run_id: "execution-110".into(),
                progress: serde_json::json!({"status":"running"}),
                reason: "progress".into(),
            }),
        )
        .unwrap();
    handle.reconcile().unwrap();
    storage.close();
    let db = Database::open(&config).unwrap();
    assert!(
        db.task("execution-1").unwrap().is_some(),
        "execution projection exists"
    );
    let tasks = db.durable_tasks().unwrap();
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        ["run-99", "work-item"]
    );
    assert_eq!(tasks[0].status, TaskStatus::Pending);
    assert_eq!(tasks[1].status, TaskStatus::Running);
}
