mod support;
#[path = "support/terminal_fence.rs"]
mod terminal_fence;

use std::sync::Arc;

use event_bus::{Event, EventBus, EventKind, LifecycleEvent, OrchestratorEvent};
use providers::FinishReason;
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

    // Then: the durable task keeps the exact failure, saved artifact and initial input.
    let record = db.task("task").expect("query").expect("durable worker");
    assert_eq!(
        record.failure_reason.as_deref(),
        Some("model response reached length limit")
    );
    assert_eq!(record.last_artifact.as_deref(), Some("validated patch"));
    assert_eq!(record.input.as_deref(), Some("WORK"));
    let progress: serde_json::Value =
        serde_json::from_str(record.progress.as_deref().expect("progress")).expect("payload");
    assert!(progress.get("resume_cursor").is_none());
    assert!(progress["input"].is_null());
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

#[tokio::test]
async fn large_conversation_does_not_inflate_task_progress() {
    // Given: repeated large tool results, but a short task prompt and final artifact.
    let file = tempfile::NamedTempFile::new().expect("file");
    std::fs::write(file.path(), "conversation evidence ".repeat(512)).expect("write");
    let mut script = (0..8)
        .map(|index| {
            Ok(support::tool_response(
                &index.to_string(),
                "read",
                serde_json::json!({"path": file.path()}),
            ))
        })
        .collect::<Vec<_>>();
    script.push(Ok(text_response("done", FinishReason::Stop)));
    let bus = Arc::new(EventBus::new(4096));
    let mut events = bus.subscribe();
    let model = Arc::new(ScriptedModel::new(script));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        model.clone(),
    );
    let run = runtime.delegate_background(
        Role::Worker,
        "read evidence".into(),
        RunConfig {
            task_id: Some("large-conversation".into()),
            budget: runtime::budget_tracker::BudgetSettings {
                max_identical_tool_call_repeats: 10,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert_eq!(
        runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    let observed = model.observed().await;
    assert_eq!(observed.len(), 9);
    let conversation_bytes = serde_json::to_vec(observed.last().unwrap()).unwrap().len();
    assert!(
        conversation_bytes > 64 * 1024,
        "conversation: {conversation_bytes} bytes"
    );
    bus.emit(Event::new(LifecycleEvent::Started {
        session_id: "flush-marker".into(),
    }));

    // Then: both execution boundaries stay small, independently of conversation size.
    let mut sizes = Vec::new();
    while let Ok(event) = events.recv().await {
        if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::Started { session_id }) if session_id == "flush-marker")
        {
            break;
        }
        if let EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { progress, .. }) =
            &event.kind
        {
            assert!(progress.get("resume_cursor").is_none());
            assert_eq!(
                progress["input"].as_str(),
                sizes.is_empty().then_some("read evidence")
            );
            let size = serde_json::to_vec(&event).unwrap().len();
            assert!(size < 3 * 1024, "TaskProgressed grew to {size} bytes");
            sizes.push(size);
        }
    }
    assert_eq!(sizes.len(), 2);
    eprintln!("conversation={conversation_bytes} bytes; TaskProgressed event sizes={sizes:?}");
}

#[tokio::test]
async fn large_json_prompt_is_input_only_once_not_a_continuation() {
    // A JSON line in user input must not be interpreted as an old supervisor continuation.
    let prompt = serde_json::json!({
        "status": "retrying",
        "input": "not the task input",
        "attempts": 99,
        "last_artifact": "untrusted artifact",
        "padding": "x".repeat(64 * 1024),
    })
    .to_string();
    let bus = Arc::new(EventBus::new(512));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        Arc::new(ScriptedModel::new([Ok(text_response(
            "",
            FinishReason::Stop,
        ))])),
    );
    let run = runtime.delegate_background(
        Role::Worker,
        prompt.clone(),
        RunConfig {
            task_id: Some("large-prompt".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    bus.emit(Event::new(LifecycleEvent::Started {
        session_id: "flush-marker".into(),
    }));
    let mut count = 0;
    while let Ok(event) = events.recv().await {
        if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::Started { session_id }) if session_id == "flush-marker")
        {
            break;
        }
        if let EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { progress, .. }) =
            &event.kind
        {
            let task: storage::entity::TaskContinuation =
                serde_json::from_value(progress.clone()).unwrap();
            assert_eq!(
                task.input.as_deref(),
                (count == 0).then_some(prompt.as_str())
            );
            assert_eq!(task.attempts, 0);
            assert!(task.last_artifact.is_none());
            assert!(progress.get("resume_cursor").is_none());
            if count > 0 {
                assert!(serde_json::to_vec(&event).unwrap().len() < 3 * 1024);
            }
            count += 1;
        }
    }
    assert_eq!(count, 2);
}

#[tokio::test]
async fn waiting_and_resumed_execution_publish_one_progress_per_transition() {
    let review_storage = support::ReviewStorage::new();
    let bus = Arc::new(EventBus::new(512));
    let mut receiver = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response("first answer", FinishReason::Stop)),
        Ok(text_response("second answer", FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        model,
    );
    review_storage.attach(&runtime);
    let run = runtime.delegate_background(
        Role::Worker,
        "initial question".into(),
        RunConfig {
            interactive: true,
            task_id: Some("interactive-task".into()),
            ..Default::default()
        },
    );
    let mut transitions = Vec::new();
    let mut progress = Vec::new();
    loop {
        let event = receiver.recv().await.unwrap();
        match event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { to, .. }) => {
                if to == event_bus::AgentRunPhase::Pending {
                    continue;
                }
                assert!(
                    !matches!(
                        to,
                        event_bus::AgentRunPhase::Error | event_bus::AgentRunPhase::Stopped
                    ),
                    "interactive worker unexpectedly terminated: {to:?}"
                );
                transitions.push(to);
                if to == event_bus::AgentRunPhase::Waiting {
                    runtime
                        .send_message(run, "follow-up question".into())
                        .unwrap();
                }
                if to == event_bus::AgentRunPhase::Done {
                    break;
                }
            }
            EventKind::Orchestrator(OrchestratorEvent::TaskProgressed {
                progress: task, ..
            }) => {
                assert!(task.get("resume_cursor").is_none());
                assert_eq!(
                    task["input"].as_str(),
                    progress.is_empty().then_some("initial question")
                );
                progress.push(task);
            }
            _ => {}
        }
    }
    assert_eq!(
        runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    assert_eq!(
        transitions,
        [
            event_bus::AgentRunPhase::Running,
            event_bus::AgentRunPhase::Waiting,
            event_bus::AgentRunPhase::Running,
            event_bus::AgentRunPhase::Done,
        ]
    );
    assert_eq!(progress.len(), transitions.len());
    assert_eq!(progress.last().unwrap()["last_artifact"], "second answer");
}
