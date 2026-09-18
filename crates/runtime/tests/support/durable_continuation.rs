use super::Fixture;
use event_bus::{Event, EventKind, OrchestratorEvent, SuppressReason};
use runtime::orchestration::ledger::GoalLedger;
use serde_json::json;
use storage::{Database, Storage, StorageConfig};

async fn seed(fixture: &Fixture) {
    let child = fixture
        .runtime
        .delegate_background_as_child(
            fixture.root,
            runtime::Role::Worker,
            "saved task",
            runtime::RunConfig::default(),
        )
        .unwrap();
    fixture.bus.emit(Event::new(OrchestratorEvent::RunAttached {
        goal_id: fixture.goal_id.clone(),
        run_id: child.to_string(),
        parent_run_id: Some(fixture.root.to_string()),
        role: "worker".into(),
        purpose: event_bus::RunPurpose::Implement,
    }));
    fixture
        .bus
        .emit(Event::new(OrchestratorEvent::TaskProgressed {
            task_id: "durable-task".into(),
            run_id: child.to_string(),
            progress: json!({
                "status": "failed", "input": "implement saved task",
                "resume_cursor": "{\"step\":7}", "last_artifact": "patch-7",
                "failure_reason": "interrupted", "attempts": 0
            }),
            reason: "interrupted".into(),
        }));
    fixture.settle().await;
}

fn persist(fixture: &Fixture, storage: &Storage) {
    storage
        .handle()
        .append_event(
            Some("session-1"),
            &Event::new(event_bus::LifecycleEvent::Started {
                session_id: "session-1".into(),
            }),
        )
        .unwrap();
    for event in fixture.orchestrator_events() {
        storage
            .handle()
            .append_event(Some("session-1"), &Event::new(event))
            .unwrap();
    }
    storage.handle().reconcile().unwrap();
}

pub async fn resume_after_interruption() {
    // Given: actual SQLite history containing the interrupted cursor and artifact.
    let fixture = Fixture::new(3).await;
    seed(&fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: temp.path().join("resume.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    persist(&fixture, &storage);
    storage.close();
    let database = Database::open(&config).unwrap();
    let events = database.events_all_ordered().unwrap();
    let replayed =
        GoalLedger::replay_checked(events.iter().filter_map(|event| match &event.event.kind {
            EventKind::Orchestrator(event) => Some(event),
            _ => None,
        }))
        .unwrap();
    let saved = replayed[&fixture.goal_id].snapshot().clone();
    let old_run = saved.task_runs["durable-task"].clone();
    let fresh = Fixture::new(3).await;
    fresh.handle.adopt(vec![(saved, vec![])]).unwrap();
    fresh.settle().await;

    // When: resume by durable identity, never restore the old tool execution.
    fresh.handle.resume_task("durable-task").unwrap();
    fresh.settle().await;

    // Then: a fresh generation retains the persisted continuation state.
    let current = fresh.handle.snapshot(&fixture.goal_id).unwrap();
    let progress = &current.task_progress["durable-task"];
    assert_eq!(progress["resume_cursor"], "{\"step\":7}");
    assert_eq!(progress["last_artifact"], "patch-7");
    assert_eq!(progress["status"], "running");
    assert_eq!(current.task_attempts["durable-task"], 1);
    assert_ne!(current.task_runs["durable-task"], old_run);
    let attached = current
        .attached_runs
        .iter()
        .find(|run| run.run_id == current.task_runs["durable-task"])
        .unwrap();
    assert_eq!(
        attached.parent_run_id.as_deref(),
        Some(fixture.root.to_string().as_str())
    );
    assert_eq!(attached.role, "worker");
    let observed = fresh.model.observed().await;
    let context = observed
        .iter()
        .flat_map(|messages| messages.iter())
        .filter(|message| message.role == providers::Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            providers::ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .find_map(|text| {
            text.lines().find_map(|line| {
                serde_json::from_str::<storage::entity::TaskContinuation>(line).ok()
            })
        })
        .expect("typed continuation reaches model");
    assert_eq!(context.resume_cursor.as_deref(), Some("{\"step\":7}"));
    assert_eq!(context.input.as_deref(), Some("implement saved task"));
    assert_eq!(
        database
            .task("durable-task")
            .unwrap()
            .unwrap()
            .resume_cursor
            .as_deref(),
        Some("{\"step\":7}")
    );
}

pub async fn retry_to_cap() {
    // Given: failed task, two allowed generations.
    let fixture = Fixture::new(2).await;
    seed(&fixture).await;
    // When: duplicate requests for each failed generation, then exceed the cap.
    for attempt in 1..=2 {
        fixture.handle.retry_task("durable-task").unwrap();
        fixture.handle.retry_task("durable-task").unwrap();
        fixture.settle().await;
        let snapshot = fixture.handle.snapshot(&fixture.goal_id).unwrap();
        assert_eq!(snapshot.task_attempts["durable-task"], attempt);
        let previous = fixture.root.to_string();
        fixture
            .bus
            .emit(Event::new(OrchestratorEvent::TaskProgressed {
                task_id: "durable-task".into(),
                run_id: previous,
                progress: json!({"obsolete": true}),
                reason: "late old generation".into(),
            }));
        fixture.settle().await;
        assert_eq!(
            fixture
                .handle
                .snapshot(&fixture.goal_id)
                .unwrap()
                .task_progress,
            snapshot.task_progress
        );
        let mut progress = snapshot.task_progress["durable-task"].clone();
        progress["status"] = json!("failed");
        fixture
            .bus
            .emit(Event::new(OrchestratorEvent::TaskProgressed {
                task_id: "durable-task".into(),
                run_id: snapshot.task_runs["durable-task"].clone(),
                progress,
                reason: "failed again".into(),
            }));
        fixture.settle().await;
    }
    fixture.handle.retry_task("durable-task").unwrap();
    fixture.settle().await;
    // Then: exactly two dispatches and a durable suppression, not a third run.
    let snapshot = fixture.handle.snapshot(&fixture.goal_id).unwrap();
    assert_eq!(snapshot.task_retries.len(), 2);
    assert_eq!(snapshot.task_attempts["durable-task"], 2);
    assert!(fixture.orchestrator_events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ContinuationSuppressed {
            reason: SuppressReason::LimitReached { max: 2 },
            ..
        }
    )));
}

pub async fn cancel_persisted() {
    // Given: failed durable task with resumable state.
    let fixture = Fixture::new(3).await;
    seed(&fixture).await;
    // When: cancel and then attempt a retry.
    fixture.handle.cancel_task("durable-task").unwrap();
    fixture.settle().await;
    fixture.handle.retry_task("durable-task").unwrap();
    fixture.settle().await;
    let temp = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: temp.path().join("cancel.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    persist(&fixture, &storage);
    storage.close();
    // Then: cancellation survives reopen without losing the cursor or retrying.
    let record = Database::open(&config)
        .unwrap()
        .task("durable-task")
        .unwrap()
        .unwrap();
    assert_eq!(record.status, storage::entity::TaskStatus::Cancelled);
    assert_eq!(record.resume_cursor.as_deref(), Some("{\"step\":7}"));
    assert!(
        fixture
            .handle
            .snapshot(&fixture.goal_id)
            .unwrap()
            .task_retries
            .is_empty()
    );
}
