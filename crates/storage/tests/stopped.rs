//! Operator stop remains durable without closing either history or task progress.

use event_bus::{Event, OrchestratorEvent};
use storage::entity::{TaskContinuation, TaskStatus};
use storage::{Database, RunContextRecord, Storage, StorageConfig};

#[test]
fn stopped_status_round_trips_sql_and_json() {
    assert_eq!(TaskStatus::Stopped.as_str(), "stopped");
    assert_eq!(TaskStatus::from_str("stopped"), Some(TaskStatus::Stopped));
    assert_eq!(
        serde_json::to_string(&TaskStatus::Stopped).unwrap(),
        "\"stopped\""
    );
    assert_eq!(
        serde_json::from_str::<TaskStatus>("\"stopped\"").unwrap(),
        TaskStatus::Stopped
    );
}

#[test]
fn latest_named_history_includes_stopped_but_not_running_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("stopped.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let mut record = RunContextRecord {
        run_id: "run-1".into(),
        role: "orchestrator".into(),
        name: "conversation".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: "[]".into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: 1,
    };
    handle.upsert_run_context(&record).unwrap();
    record.run_id = "run-2".into();
    record.terminal_phase = "Stopped".into();
    record.updated_at_ns = 2;
    handle.upsert_run_context(&record).unwrap();
    let stopped = record.clone();
    record.run_id = "run-3".into();
    record.terminal_phase = "Running".into();
    record.updated_at_ns = 3;
    handle.upsert_run_context(&record).unwrap();
    storage.close();
    let db = Database::open(&config).unwrap();
    assert_eq!(
        db.latest_terminal_run_context("conversation").unwrap(),
        Some(stopped)
    );
}

#[test]
fn durable_projection_keeps_stopped_task_reopenable() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("tasks.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let mut task = TaskContinuation {
        status: TaskStatus::Running,
        input: Some("work".into()),
        resume_cursor: Some("saved cursor".into()),
        last_artifact: Some("partial artifact".into()),
        failure_reason: None,
        attempts: 1,
        heartbeat_at_ns: Some(1),
    };
    for status in [
        TaskStatus::Running,
        TaskStatus::Stopped,
        TaskStatus::Running,
        TaskStatus::Completed,
    ] {
        task.status = status;
        task.failure_reason = (status == TaskStatus::Stopped).then(|| "stopped".into());
        handle
            .append_event(
                None,
                &Event::new(OrchestratorEvent::TaskProgressed {
                    task_id: "task".into(),
                    run_id: "run-1".into(),
                    progress: serde_json::to_value(&task).unwrap(),
                    reason: "task execution boundary".into(),
                }),
            )
            .unwrap();
        handle.reconcile().unwrap();
        let db = Database::open(&config).unwrap();
        let saved = db.task("task").unwrap().unwrap();
        assert_eq!(saved.status, status);
        assert_eq!(saved.resume_cursor, task.resume_cursor);
        assert_eq!(saved.last_artifact, task.last_artifact);
        assert_eq!(saved.failure_reason, task.failure_reason);
    }
    storage.close();
}
