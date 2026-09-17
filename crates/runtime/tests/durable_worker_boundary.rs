mod support;
#[path = "support/terminal_fence.rs"]
mod terminal_fence;

use std::sync::Arc;

use event_bus::{Event, EventBus, EventKind, LifecycleEvent, OrchestratorEvent, RunPurpose};
use providers::{ContentBlock, FinishReason};
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::{GoalLedger, OrchestrationSettings};
use runtime::orchestration::supervisor::{GoalSpec, GoalSupervisor, SupervisorHandle};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use storage::entity::TaskContinuation;
use storage::{Database, Storage, StorageConfig};
use support::{ScriptedModel, text_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

struct Fixture {
    bus: Arc<EventBus>,
    runtime: AgentRuntime,
    handle: SupervisorHandle,
    model: Arc<ScriptedModel>,
    goal: String,
    worker: runtime::RunId,
    events: event_bus::EventReceiver,
}

impl Fixture {
    async fn new(finish: FinishReason) -> Self {
        let bus = Arc::new(EventBus::new(512));
        let events = bus.subscribe();
        let model = Arc::new(ScriptedModel::new([]));
        model.gate_key("ROOT", Arc::new(Notify::new())).await;
        model.gate_key("Continue", Arc::new(Notify::new())).await;
        model
            .add_keyed(
                "WORK",
                [
                    Ok(text_response("validated patch", FinishReason::ToolUse)),
                    Ok(text_response(
                        if matches!(finish, FinishReason::Length) {
                            "truncated invalid patch"
                        } else {
                            ""
                        },
                        finish,
                    )),
                ],
            )
            .await;
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::with_standard_tools(
                bus.clone(),
                Arc::new(DirectSandbox::new_unchecked()),
            )),
            model.clone(),
        );
        let handle = GoalSupervisor::spawn(
            runtime.clone(),
            bus.clone(),
            Arc::new(FixtureDeliveryAdapter::default()),
            OrchestrationSettings::default(),
        );
        let root =
            runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
        let goal = handle.create_goal(
            GoalSpec {
                session_id: "session".into(),
                project_id: "project".into(),
                thread_id: "thread".into(),
                goal: "repair".into(),
                references: vec![],
                constraints: vec![],
                repo: "repo".into(),
                base_ref: "main".into(),
            },
            root,
        );
        let worker = runtime
            .delegate_background_as_child(
                root,
                Role::Worker,
                "WORK",
                RunConfig {
                    task_id: Some("task".into()),
                    ..RunConfig::default()
                },
            )
            .expect("worker");
        bus.emit(Event::new(OrchestratorEvent::RunAttached {
            goal_id: goal.clone(),
            run_id: worker.to_string(),
            parent_run_id: Some(root.to_string()),
            role: "worker".into(),
            purpose: RunPurpose::Implement,
        }));
        Self {
            bus,
            runtime,
            handle,
            model,
            goal,
            worker,
            events,
        }
    }

    async fn finished(&self) {
        self.runtime
            .wait(self.worker)
            .await
            .expect("worker terminates");
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }
}

async fn persist(fixture: &mut Fixture, config: &StorageConfig) -> Database {
    let storage = Storage::open(config.clone()).expect("open SQLite");
    storage
        .handle()
        .append_event(
            Some("session"),
            &Event::new(LifecycleEvent::Started {
                session_id: "session".into(),
            }),
        )
        .expect("session");
    fixture.bus.emit(Event::new(LifecycleEvent::Started {
        session_id: "flush-marker".into(),
    }));
    while let Ok(event) = fixture.events.recv().await {
        if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::Started { session_id }) if session_id == "flush-marker")
        {
            break;
        }
        storage
            .handle()
            .append_event(Some("session"), &event)
            .expect("persist real bus event");
    }
    storage.handle().reconcile().expect("reconcile");
    storage.close();
    let reopened = Storage::open(config.clone()).expect("reopen");
    reopened
        .handle()
        .reconcile()
        .expect("reconcile after reopen");
    reopened.close();
    Database::open(config).expect("read SQLite")
}

#[tokio::test]
async fn real_worker_failure_survives_sqlite_and_resumes_with_saved_work() {
    // Given: a worker actually produces an artifact, then reaches a model limit.
    let mut fixture = Fixture::new(FinishReason::Length).await;
    fixture.finished().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let config = StorageConfig {
        db_path: temp.path().join("tasks.db"),
        ..StorageConfig::default()
    };

    // When: replay the persisted execution into a new supervisor and resume by task id.
    let db = persist(&mut fixture, &config).await;
    let record = db.task("task").expect("query").expect("durable worker");
    assert_eq!(
        record.failure_reason.as_deref(),
        Some("model response reached length limit")
    );
    assert_eq!(record.last_artifact.as_deref(), Some("validated patch"));
    let cursor: Vec<providers::Message> =
        serde_json::from_str(record.resume_cursor.as_deref().expect("meaningful cursor"))
            .expect("saved messages");
    assert!(
        cursor
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "validated patch"))
    );
    let stored = db.events_all_ordered().expect("events");
    let replay =
        GoalLedger::replay_checked(stored.iter().filter_map(|event| match &event.event.kind {
            EventKind::Orchestrator(event) => Some(event),
            _ => None,
        }))
        .expect("replay");
    let saved = replay[&fixture.goal].snapshot().clone();
    let fresh = Fixture::new(FinishReason::Length).await;
    fresh.finished().await;
    let resumed_handle = GoalSupervisor::spawn(
        fresh.runtime.clone(),
        fresh.bus.clone(),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    resumed_handle.adopt(vec![(saved, vec![])]).expect("adopt");
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    resumed_handle.resume_task("task").expect("resume");
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }

    // Then: new generation receives the real saved cursor/artifact and exact failure.
    let current = resumed_handle.snapshot(&fixture.goal).expect("snapshot");
    assert_ne!(current.task_runs["task"], fixture.worker.to_string());
    assert_eq!(current.task_attempts["task"], 1);
    let observed = fresh.model.observed().await;
    let resumed = observed
        .iter()
        .flat_map(|messages| messages.iter())
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .flat_map(|text| text.lines())
        .find_map(|line| serde_json::from_str::<TaskContinuation>(line).ok())
        .expect("continuation at model boundary");
    assert_eq!(resumed.resume_cursor, record.resume_cursor);
    assert_eq!(resumed.last_artifact, record.last_artifact);
    assert_eq!(resumed.failure_reason, record.failure_reason);
    let progress: TaskContinuation =
        serde_json::from_value(current.task_progress["task"].clone()).expect("typed state");
    assert_eq!(progress.last_artifact, record.last_artifact);
    assert_eq!(progress.attempts, 1);
}
