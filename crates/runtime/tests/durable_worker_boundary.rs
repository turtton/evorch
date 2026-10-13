mod support;
#[path = "support/terminal_fence.rs"]
mod terminal_fence;

use std::sync::Arc;

use event_bus::{Event, EventBus, EventKind, LifecycleEvent, OrchestratorEvent};
use providers::{ContentBlock, FinishReason};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use storage::{Database, Storage, StorageConfig};
use support::{ScriptedModel, text_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

/// How the durable worker reaches its terminal state.
enum Ending {
    Finish(FinishReason),
    Cancel,
}

struct Fixture {
    bus: Arc<EventBus>,
    runtime: AgentRuntime,
    worker: runtime::RunId,
    events: event_bus::EventReceiver,
}

impl Fixture {
    async fn new(ending: Ending) -> Self {
        let bus = Arc::new(EventBus::new(512));
        let events = bus.subscribe();
        let model = Arc::new(ScriptedModel::new([]));
        let cancel = matches!(ending, Ending::Cancel);
        match ending {
            Ending::Finish(finish) => {
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
            }
            Ending::Cancel => model.gate_key("WORK", Arc::new(Notify::new())).await,
        }
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::with_standard_tools(
                bus.clone(),
                Arc::new(DirectSandbox::new_unchecked()),
            )),
            model.clone(),
        );
        let worker = runtime.delegate_background(
            Role::Worker,
            "WORK".into(),
            RunConfig {
                task_id: Some("task".into()),
                ..RunConfig::default()
            },
        );
        if cancel {
            model.wait_for_request(0).await;
            runtime.cancel(worker).expect("cancel worker");
        }
        runtime.wait(worker).await.expect("worker terminates");
        Self {
            bus,
            runtime,
            worker,
            events,
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
async fn real_worker_failure_survives_sqlite_with_saved_work() {
    // Given: a worker actually produces an artifact, then reaches a model limit.
    let mut fixture = Fixture::new(Ending::Finish(FinishReason::Length)).await;
    let temp = tempfile::tempdir().expect("tempdir");
    let config = StorageConfig {
        db_path: temp.path().join("tasks.db"),
        ..StorageConfig::default()
    };

    // When: the real bus events are persisted, reconciled and reopened.
    let db = persist(&mut fixture, &config).await;

    // Then: the durable task keeps the exact failure, saved artifact and cursor.
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
}

#[tokio::test]
async fn boundary_events_are_not_published_for_runs_without_durable_task_identity() {
    // Given: an identity-less worker run, like the chat completion path — no
    // task_id / team_task.
    let bus = Arc::new(EventBus::new(512));
    let mut events = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "",
        FinishReason::Stop,
    ))]));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        model,
    );
    let run = runtime.delegate_background(Role::Worker, "prompt".into(), RunConfig::default());
    runtime.wait(run).await.expect("run terminates");
    bus.emit(Event::new(LifecycleEvent::Started {
        session_id: "flush-marker".into(),
    }));

    // When: draining everything the run emitted.
    // Then: no TaskProgressed boundary was published for a run that is not a
    // durable task.
    while let Ok(event) = events.recv().await {
        if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::Started { session_id }) if session_id == "flush-marker")
        {
            break;
        }
        assert!(
            !matches!(
                event.kind,
                EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { .. })
            ),
            "identity-less run leaked a boundary event: {event:?}"
        );
    }
}
