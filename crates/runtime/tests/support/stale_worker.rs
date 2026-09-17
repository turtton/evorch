use super::Fixture;
use event_bus::{Event, OrchestratorEvent, RunPurpose};
use serde_json::json;

pub async fn transitions_to_retrying() {
    // Given: an attached worker whose persisted heartbeat is older than the TTL.
    let fixture = Fixture::new(3).await;
    let mut events = fixture.handle.subscribe();
    let child = fixture
        .runtime
        .delegate_background_as_child(
            fixture.root,
            runtime::Role::Worker,
            "stale task",
            runtime::RunConfig::default(),
        )
        .expect("worker");
    let old_run = child.to_string();
    fixture.bus.emit(Event::new(OrchestratorEvent::RunAttached {
        goal_id: fixture.goal_id.clone(),
        run_id: old_run.clone(),
        parent_run_id: Some(fixture.root.to_string()),
        role: "worker".into(),
        purpose: RunPurpose::Implement,
    }));
    fixture
        .bus
        .emit(Event::new(OrchestratorEvent::TaskProgressed {
            task_id: "stale-task".into(),
            run_id: old_run.clone(),
            progress: json!({"status":"running", "input":"saved input", "resume_cursor":"step-7",
            "last_artifact":"patch-7", "failure_reason":null, "attempts":1, "heartbeat_at_ns":1}),
            reason: "saved progress".into(),
        }));
    fixture.settle().await;

    // When: the existing supervisor sampler observes the expired heartbeat.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.expect("event stream");
            if matches!(event.kind, event_bus::EventKind::Orchestrator(
                OrchestratorEvent::TaskRetryScheduled { ref task_id, .. }) if task_id == "stale-task") {
                break;
            }
        }
    }).await.expect("stale worker must be retried by the sampler");
    fixture.settle().await;

    // Then: exactly one fresh generation retains identity, parent and cursor.
    let snapshot = fixture.handle.snapshot(&fixture.goal_id).expect("snapshot");
    assert_eq!(
        snapshot.stale_marks,
        vec![("stale-task".into(), old_run.clone(), 1)]
    );
    assert_eq!(snapshot.task_retries.len(), 1);
    assert_eq!(snapshot.task_attempts["stale-task"], 2);
    let new_run = &snapshot.task_runs["stale-task"];
    assert_ne!(new_run, &old_run);
    let progress = &snapshot.task_progress["stale-task"];
    assert_eq!(progress["status"], "retrying");
    assert_eq!(progress["failure_reason"], "stale-worker");
    assert_eq!(progress["resume_cursor"], "step-7");
    let attached = snapshot
        .attached_runs
        .iter()
        .find(|run| &run.run_id == new_run)
        .expect("retry attached");
    assert_eq!(attached.parent_run_id, Some(fixture.root.to_string()));
    assert!(
        fixture
            .orchestrator_events()
            .iter()
            .any(|event| matches!(event,
        OrchestratorEvent::TaskRetryScheduled { task_id, attempt: 2, reason, new_run_id }
        if task_id == "stale-task" && reason == "stale-worker" && new_run_id == new_run))
    );
    let temp = tempfile::tempdir().expect("temporary database");
    let config = storage::StorageConfig {
        db_path: temp.path().join("stale.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).expect("storage");
    storage
        .handle()
        .append_event(
            Some("session-1"),
            &Event::new(event_bus::LifecycleEvent::Started {
                session_id: "session-1".into(),
            }),
        )
        .expect("session");
    for event in fixture.orchestrator_events() {
        storage
            .handle()
            .append_event(Some("session-1"), &Event::new(event))
            .expect("persist event");
    }
    storage.handle().reconcile().expect("projection");
    storage.close();
    let record = storage::Database::open(&config)
        .expect("reopen")
        .task("stale-task")
        .expect("task query")
        .expect("task");
    assert_eq!(record.status, storage::entity::TaskStatus::Retrying);
    assert_eq!(record.attempts, 2);
    assert_eq!(record.failure_reason.as_deref(), Some("stale-worker"));
    assert_eq!(record.parent_run_id, Some(fixture.root.to_string()));
    assert!(record.heartbeat_at.is_some_and(
        |heartbeat| heartbeat > std::time::UNIX_EPOCH + std::time::Duration::from_nanos(1)
    ));
}
