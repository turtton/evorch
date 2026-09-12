#[path = "restore/blockers.rs"]
mod blockers;
#[path = "restore/concurrent.rs"]
mod concurrent;
#[path = "restore/send.rs"]
mod send;
mod support;

use std::sync::Arc;

use config::{CompactionConfig, SummarizerKind};
use event_bus::{AgentRunPhase, CompactionEvent, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason, Message};
use runtime::{AgentRuntime, CompactionCheckpoint, Role, RunConfig, RunId, RunStore};
use sandbox::DirectSandbox;
use storage::{Database, Storage, StorageConfig};
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
}

fn storage_fixture() -> (tempfile::TempDir, StorageConfig, Storage, Database) {
    let dir = tempfile::tempdir().expect("temp directory");
    let config = StorageConfig {
        db_path: dir.path().join("runs.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("writer");
    let database = Database::open(&config).expect("reader");
    (dir, config, storage, database)
}

#[tokio::test]
async fn send_to_done_run_preserves_uncompacted_history() {
    // Given: a completed child with a durable conversation.
    let (_dir, config, storage, database) = storage_fixture();
    let model = Arc::new(ScriptedModel::new(
        (0..3).map(|_| Ok(text_response("answer", FinishReason::Stop))),
    ));
    let (runtime, _) = runtime_with(Arc::clone(&model));
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&runtime, parent).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "original", RunConfig::default())
        .unwrap();
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
    let record = database.run_context(&child.to_string()).unwrap().unwrap();
    let original: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    // When: send starts another turn using the same run ID.
    let id = runtime
        .send_agent_message(
            parent,
            child,
            event_bus::AgentMessageKind::Send,
            "follow up",
            None,
        )
        .unwrap();
    // Then: model receives the original history followed by exactly one trigger.
    assert_eq!(id, "msg-1");
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
    let observed = model.observed().await;
    let restored = observed.last().unwrap();
    assert_eq!(&restored[..original.len()], original);
    assert_eq!(restored.len(), original.len() + 1);
    assert_eq!(restored.last().unwrap().role, providers::Role::User);
}

async fn terminal(runtime: &AgentRuntime, run_id: RunId) -> AgentRunPhase {
    timeout(Duration::from_secs(5), runtime.wait(run_id))
        .await
        .expect("termination deadline")
        .expect("known run")
}

#[tokio::test]
async fn send_to_done_run_restores_full_context_and_processes_new_turn() {
    // Given: an interactive run with compactable raw history and a real SQLite writer.
    let (_dir, config, storage, database) = storage_fixture();
    let prompt = "initial request ".repeat(40);
    let first_reply = text_response(&"old answer ".repeat(40), FinishReason::Stop);
    let final_reply = text_response("done", FinishReason::Stop);
    let model = Arc::new(ScriptedModel::new([
        Ok(first_reply.clone()),
        Ok(final_reply.clone()),
        Ok(text_response("sender", FinishReason::Stop)),
        Ok(text_response("restored", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let runtime = runtime
        .with_run_store(RunStore::open(&config, storage.handle()).expect("run store"))
        .with_compaction(CompactionConfig {
            context_window_tokens: 1_000_000,
            keep_recent_tokens: 1,
            max_summary_bytes: 64,
            summarizer: SummarizerKind::Structural,
            ..CompactionConfig::default()
        });
    let mut events = bus.subscribe();
    let run_id = runtime.delegate_background(
        Role::Worker,
        prompt.clone(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.expect("event").kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Waiting,
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .expect("waiting deadline");
    assert!(
        database
            .run_context(&run_id.to_string())
            .expect("read")
            .is_none()
    );

    // When: manual compaction precedes the resumed turn's terminal response.
    runtime.compact(run_id).expect("compact");
    runtime
        .send_message(run_id, "continue".into())
        .expect("resume");
    assert_eq!(terminal(&runtime, run_id).await, AgentRunPhase::Done);

    // Then: the complete raw history and verbatim checkpoint survive independently.
    let record = database
        .run_context(&run_id.to_string())
        .expect("read")
        .expect("snapshot");
    let messages: Vec<Message> = serde_json::from_str(&record.messages_json).expect("messages");
    let checkpoints: Vec<CompactionCheckpoint> =
        serde_json::from_str(&record.checkpoints_json).expect("checkpoints");
    assert_eq!(
        messages[1..],
        vec![
            Message {
                role: providers::Role::User,
                content: vec![ContentBlock::Text { text: prompt }]
            },
            first_reply.message,
            Message {
                role: providers::Role::User,
                content: vec![ContentBlock::Text {
                    text: "continue".into()
                }]
            },
            final_reply.message,
        ]
    );
    assert_eq!(messages[0], model.observed().await[0][0]);
    assert_eq!(checkpoints.len(), 1);
    let summary = timeout(Duration::from_secs(5), async {
        loop {
            if let EventKind::Compaction(CompactionEvent::Compacted { summary, .. }) =
                events.recv().await.expect("event").kind
            {
                break summary;
            }
        }
    })
    .await
    .expect("compaction event");
    assert_eq!(
        checkpoints[0].summary.content,
        vec![ContentBlock::Text {
            text: format!("[COMPACTION CHECKPOINT {}]\n{summary}", checkpoints[0].id),
        }]
    );
    assert_eq!(record.terminal_phase, "Done");
    assert!(record.restorable);
    assert!(record.updated_at_ns > 0);

    let sender = runtime
        .delegate_background_as_child(run_id, Role::Worker, "sender", RunConfig::default())
        .unwrap();
    terminal(&runtime, sender).await;
    runtime
        .send_agent_message(
            sender,
            run_id,
            event_bus::AgentMessageKind::Send,
            "new turn",
            None,
        )
        .unwrap();
    assert_eq!(terminal(&runtime, run_id).await, AgentRunPhase::Done);
    let observed = model.observed().await;
    let restored = observed.last().unwrap();
    let checkpoint = &checkpoints[0];
    let mut expected = messages[..checkpoint.range.0].to_vec();
    expected.push(checkpoint.summary.clone());
    expected.extend_from_slice(&messages[checkpoint.range.1..]);
    assert_eq!(&restored[..expected.len()], expected);
    assert_eq!(restored.len(), expected.len() + 1);
}

#[tokio::test]
async fn runs_without_run_store_persist_nothing_and_terminate_normally() {
    // Given: storage exists, but is not attached to the runtime.
    let (_dir, _config, _storage, database) = storage_fixture();
    let (runtime, _) = runtime_with(Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))])));
    // When: a normal run terminates.
    let run = runtime.delegate_background(Role::Worker, "request".into(), RunConfig::default());
    // Then: normal termination does not create a snapshot.
    assert_eq!(terminal(&runtime, run).await, AgentRunPhase::Done);
    assert!(
        database
            .run_context(&run.to_string())
            .expect("read")
            .is_none()
    );
}

#[tokio::test]
async fn team_or_ownership_run_captured_as_not_restorable() {
    // Given: a run holding a live team board.
    let (_dir, config, storage, database) = storage_fixture();
    let (runtime, _) = runtime_with(Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))])));
    let runtime =
        runtime.with_run_store(RunStore::open(&config, storage.handle()).expect("run store"));
    // When: the team run terminates.
    let run = runtime.delegate_background(
        Role::Orchestrator,
        "request".into(),
        RunConfig {
            topology: runtime::CoordinationTopology::DynamicTeam { max_workers: 1 },
            delegation_value: Some("parallel review".into()),
            team_store: Some(runtime::team_context::TeamStore {
                config: config.clone(),
                writer: storage.handle(),
                id: "snapshot-team".into(),
            }),
            ..RunConfig::default()
        },
    );
    assert_eq!(terminal(&runtime, run).await, AgentRunPhase::Done);
    // Then: persistence records the unsupported capability instead of silently dropping it.
    let record = database
        .run_context(&run.to_string())
        .expect("read")
        .expect("snapshot");
    let descriptor: runtime::restore::RunRestoreDescriptor =
        serde_json::from_str(&record.config_json).expect("descriptor");
    assert!(!record.restorable);
    assert!(!descriptor.restorable);
    assert!(descriptor.non_restorable_reason.is_some());
}

#[tokio::test]
async fn error_transition_persists_context_snapshot() {
    // Given: a provider failure and a configured run store.
    let (_dir, config, storage, database) = storage_fixture();
    let (runtime, _) = runtime_with(Arc::new(ScriptedModel::new([Err(
        runtime::RuntimeError::Model {
            reason: "failure".into(),
        },
    )])));
    let runtime =
        runtime.with_run_store(RunStore::open(&config, storage.handle()).expect("run store"));
    // When: model completion fails.
    let run = runtime.delegate_background(Role::Worker, "request".into(), RunConfig::default());
    assert_eq!(terminal(&runtime, run).await, AgentRunPhase::Error);
    // Then: the error run's input is captured too.
    let record = database
        .run_context(&run.to_string())
        .expect("read")
        .expect("snapshot");
    assert_eq!(record.terminal_phase, "Error");
    let messages: Vec<Message> = serde_json::from_str(&record.messages_json).expect("messages");
    assert_eq!(
        messages[0].content,
        vec![ContentBlock::Text {
            text: "request".into()
        }]
    );
}

#[tokio::test]
async fn closed_writer_does_not_block_run_termination() {
    // Given: the configured storage writer has closed.
    let (_dir, config, storage, database) = storage_fixture();
    let (runtime, _) = runtime_with(Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))])));
    let runtime =
        runtime.with_run_store(RunStore::open(&config, storage.handle()).expect("run store"));
    storage.close();
    // When: the run attempts its terminal snapshot.
    let run = runtime.delegate_background(Role::Worker, "request".into(), RunConfig::default());
    // Then: storage failure does not replace normal termination.
    assert_eq!(terminal(&runtime, run).await, AgentRunPhase::Done);
    assert!(
        database
            .run_context(&run.to_string())
            .expect("read")
            .is_none()
    );
}
