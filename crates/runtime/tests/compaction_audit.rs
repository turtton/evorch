mod support;

// allow: SIZE_OK — three AC4/AC5 end-to-end scenarios share one real storage bridge and model.

use std::collections::VecDeque;
use std::fs;
use std::sync::Arc;

use agents::Role as AgentRole;
use async_trait::async_trait;
use config::{CompactionConfig, SummarizerKind};
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, CompactionEvent,
    CompactionReason, DeliveryDisposition, Event, EventBus, EventKind, LifecycleEvent,
    MessageEvent, RecvError, ToolEvent,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, Role, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, RunConfig, RuntimeError};
use sandbox::DirectSandbox;
use storage::{Database, Storage, StorageConfig, StorageHandle, StoredEvent};
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::text_response;

const TEST_SESSION: &str = "compaction-audit";

enum ModelStep {
    Reply(ChatResponse),
    Fail(RuntimeError),
    Block(Arc<Notify>, ChatResponse),
}

struct AuditModel {
    steps: Mutex<VecDeque<ModelStep>>,
    observed: Mutex<Vec<Vec<Message>>>,
}

impl AuditModel {
    fn new(steps: impl IntoIterator<Item = ModelStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into_iter().collect()),
            observed: Mutex::new(Vec::new()),
        }
    }

    async fn observed(&self) -> Vec<Vec<Message>> {
        self.observed.lock().await.clone()
    }
}

#[async_trait]
impl AgentModel for AuditModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: AgentRole,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.observed.lock().await.push(messages.to_vec());
        match self.steps.lock().await.pop_front().expect("script step") {
            ModelStep::Reply(response) => Ok(response),
            ModelStep::Fail(error) => Err(error),
            ModelStep::Block(gate, response) => {
                gate.notified().await;
                Ok(response)
            }
        }
    }

    fn selected_model(&self, role: AgentRole) -> String {
        format!("audit-{}", role.name().to_lowercase())
    }
}

fn storage_config(temp: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: temp.path().join("compaction-audit.db"),
        ..StorageConfig::default()
    }
}

fn runtime_with(
    model: Arc<AuditModel>,
    bus: Arc<EventBus>,
    settings: CompactionConfig,
) -> AgentRuntime {
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, model).with_compaction(settings)
}

fn spawn_storage_bridge(bus: &EventBus, handle: StorageHandle) -> JoinHandle<()> {
    let mut subscriber = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match subscriber.recv().await {
                Ok(event) if matches!(event.kind, EventKind::Usage(_)) => {}
                Ok(event) => handle
                    .append_event(Some(TEST_SESSION), &event)
                    .expect("bridge persists appendable event"),
                Err(RecvError::Lagged(skipped)) => panic!("storage bridge lagged by {skipped}"),
                Err(RecvError::Closed) => return,
            }
        }
    })
}

fn seed_audit_events(bus: &EventBus) -> Vec<String> {
    let transcript = vec!["alpha".to_string(), "beta\0bytes".to_string()];
    bus.emit(Event::new(LifecycleEvent::Started {
        session_id: TEST_SESSION.to_string(),
    }));
    bus.emit(Event::new(MessageEvent::MessageDelta {
        delta: transcript[0].clone(),
    }));
    bus.emit(Event::new(ToolEvent::ToolStarted {
        tool_name: "read".to_string(),
        call_id: "completed-call".to_string(),
        run_id: None,
    }));
    bus.emit(Event::new(ToolEvent::ToolCompleted {
        tool_name: "read".to_string(),
        call_id: "completed-call".to_string(),
        is_error: false,
        detail: Some(serde_json::json!({ "result": "read complete" })),
        run_id: None,
    }));
    bus.emit(Event::new(AgentMessageEvent::Delivered {
        message: AgentMessage {
            message_id: "audit-message".to_string(),
            sender_run_id: "run-source".to_string(),
            recipient_run_id: "run-target".to_string(),
            kind: AgentMessageKind::Send,
            content: "delivery payload".to_string(),
            reply_to: None,
        },
        disposition: DeliveryDisposition::Aside,
    }));
    bus.emit(Event::new(ToolEvent::ToolStarted {
        tool_name: "write".to_string(),
        call_id: "open-call".to_string(),
        run_id: None,
    }));
    bus.emit(Event::new(MessageEvent::MessageDelta {
        delta: transcript[1].clone(),
    }));
    transcript
}

async fn wait_for_phase(runtime: &AgentRuntime, run_id: runtime::RunId, phase: AgentRunPhase) {
    timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .inspect_agent(run_id)
                .expect("inspectable run")
                .phase
                == phase
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("phase transition timeout");
}

async fn wait_for_event_count(config: &StorageConfig, count: usize) -> Vec<StoredEvent> {
    timeout(Duration::from_secs(2), async {
        loop {
            let events = Database::open(config)
                .expect("reader opens")
                .events_by_session(TEST_SESSION)
                .expect("events read");
            if events.len() >= count {
                return events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("storage bridge drain timeout")
}

fn has_checkpoint(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text } if text.starts_with("[COMPACTION CHECKPOINT ckpt-"))
        })
    })
}

#[tokio::test]
async fn compaction_is_auditable_in_storage() {
    // Given: persisted transcript/projection rows and a waiting run with compactable history
    let temp = TempDir::new().expect("temporary directory");
    let config = storage_config(&temp);
    let storage = Storage::open(config.clone()).expect("storage opens");
    let bus = Arc::new(EventBus::new(256));
    let bridge = spawn_storage_bridge(&bus, storage.handle());
    let seeded = seed_audit_events(&bus);
    let gate = Arc::new(Notify::new());
    let model = Arc::new(AuditModel::new([
        ModelStep::Reply(text_response(&"old answer ".repeat(30), FinishReason::Stop)),
        ModelStep::Block(Arc::clone(&gate), text_response("done", FinishReason::Stop)),
    ]));
    let settings = CompactionConfig {
        context_window_tokens: 1_000_000,
        keep_recent_tokens: 1,
        max_summary_bytes: 128,
        summarizer: SummarizerKind::Structural,
        ..CompactionConfig::default()
    };
    let runtime = runtime_with(Arc::clone(&model), Arc::clone(&bus), settings);
    let run_id = runtime.delegate_background(
        AgentRole::Worker,
        "old request ".repeat(30),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;
    let observed_before = model.observed().await;
    let events_before = wait_for_event_count(&config, 9).await;
    let reader = Database::open(&config).expect("reader opens");
    let messages_before = reader
        .messages_by_session(TEST_SESSION)
        .expect("messages read");
    let snapshot_before = reader
        .restore_session(TEST_SESSION)
        .expect("snapshot restores")
        .expect("seed created snapshot");
    assert_eq!(
        snapshot_before.open_tool_calls,
        vec![("write".into(), "open-call".into())]
    );

    // When: manual compaction completes, while the following provider response remains blocked
    runtime.compact(run_id).expect("manual compaction accepted");
    runtime
        .send_message(run_id, "resume".to_string())
        .expect("waiting run resumes");
    timeout(Duration::from_secs(2), async {
        while model.observed().await.len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("resumed provider request timeout");
    let events_after = wait_for_event_count(&config, events_before.len() + 1).await;
    let reader = Database::open(&config).expect("reader reopens");

    // Then: prior bytes/order and projections remain exact; one complete compaction row is appended
    assert_eq!(
        &events_after[..events_before.len()],
        events_before.as_slice()
    );
    let compacted = events_after
        .iter()
        .skip(events_before.len())
        .filter(|stored| matches!(stored.event.kind, EventKind::Compaction(_)))
        .collect::<Vec<_>>();
    assert_eq!(compacted.len(), 1);
    let stored = compacted[0];
    let encoded = serde_json::to_vec(&stored.event.kind).expect("event serializes");
    assert_eq!(
        serde_json::from_slice::<EventKind>(&encoded).expect("event deserializes"),
        stored.event.kind
    );
    let EventKind::Compaction(CompactionEvent::Compacted {
        run_id: event_run_id,
        reason,
        threshold,
        context_window_tokens,
        estimated_tokens_before,
        estimated_tokens_after,
        compacted_range_start,
        compacted_range_end,
        checkpoint_id,
        summary,
    }) = &stored.event.kind
    else {
        panic!("expected one persisted compaction event")
    };
    assert_eq!(event_run_id, &run_id.to_string());
    assert_eq!(*reason, CompactionReason::Manual);
    assert_eq!(*threshold, 0.75);
    assert_eq!(*context_window_tokens, 1_000_000);
    assert!(estimated_tokens_after < estimated_tokens_before);
    assert!(*compacted_range_start < *compacted_range_end);
    assert!(*compacted_range_end <= observed_before[0].len() + 2);
    assert!(checkpoint_id.starts_with("ckpt-"));
    assert!(!summary.is_empty());
    assert_eq!(
        reader
            .messages_by_session(TEST_SESSION)
            .expect("messages reread"),
        messages_before
    );
    assert_eq!(
        reader
            .restore_session(TEST_SESSION)
            .expect("snapshot rerestores"),
        Some(snapshot_before)
    );
    assert_eq!(seeded, vec!["alpha", "beta\0bytes"]);

    gate.notify_one();
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    bridge.abort();
    drop(reader);
    storage.close();
    let db_bytes = fs::read(&config.db_path).expect("database bytes read");
    let stored_kind_and_payload =
        b"compaction{\"kind\":\"Compaction\",\"payload\":{\"kind\":\"Compacted\"";
    assert_eq!(
        db_bytes
            .windows(stored_kind_and_payload.len())
            .filter(|bytes| *bytes == stored_kind_and_payload)
            .count(),
        1
    );
}

#[tokio::test]
async fn summarizer_failure_is_atomic_end_to_end() {
    // Given: persisted rows and a model summarizer that fails before a blocked provider continuation
    let temp = TempDir::new().expect("temporary directory");
    let config = storage_config(&temp);
    let storage = Storage::open(config.clone()).expect("storage opens");
    let bus = Arc::new(EventBus::new(256));
    let bridge = spawn_storage_bridge(&bus, storage.handle());
    seed_audit_events(&bus);
    let gate = Arc::new(Notify::new());
    let model = Arc::new(AuditModel::new([
        ModelStep::Reply(text_response(&"old answer ".repeat(20), FinishReason::Stop)),
        ModelStep::Fail(RuntimeError::Model {
            reason: "summary unavailable".to_string(),
        }),
        ModelStep::Block(Arc::clone(&gate), text_response("done", FinishReason::Stop)),
    ]));
    let runtime = runtime_with(
        Arc::clone(&model),
        Arc::clone(&bus),
        CompactionConfig {
            context_window_tokens: 1_000_000,
            keep_recent_tokens: 1,
            summarizer: SummarizerKind::Model,
            ..CompactionConfig::default()
        },
    );
    let run_id = runtime.delegate_background(
        AgentRole::Worker,
        "ATOMIC-HISTORY".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, run_id, AgentRunPhase::Waiting).await;
    let events_before = wait_for_event_count(&config, 9).await;
    let reader = Database::open(&config).expect("reader opens");
    let messages_before = reader
        .messages_by_session(TEST_SESSION)
        .expect("messages read");
    let snapshot_before = reader
        .restore_session(TEST_SESSION)
        .expect("snapshot restores");

    // When: manual summarization fails and the unchanged visible window reaches the next request
    runtime.compact(run_id).expect("manual compaction accepted");
    runtime
        .send_message(run_id, "resume-after-failure".to_string())
        .expect("waiting run resumes");
    timeout(Duration::from_secs(2), async {
        while model.observed().await.len() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider continuation timeout");
    let events_after = Database::open(&config)
        .expect("reader reopens")
        .events_by_session(TEST_SESSION)
        .expect("events reread");

    // Then: no event/projection mutation escapes, and the full raw window reaches the provider
    assert_eq!(
        &events_after[..events_before.len()],
        events_before.as_slice()
    );
    assert!(
        events_after
            .iter()
            .skip(events_before.len())
            .all(|stored| !matches!(stored.event.kind, EventKind::Compaction(_)))
    );
    let reader = Database::open(&config).expect("reader reopens");
    assert_eq!(
        reader
            .messages_by_session(TEST_SESSION)
            .expect("messages reread"),
        messages_before
    );
    assert_eq!(
        reader
            .restore_session(TEST_SESSION)
            .expect("snapshot rerestores"),
        snapshot_before
    );
    let observed = model.observed().await;
    let resumed = observed.last().expect("continued provider request");
    assert!(!has_checkpoint(resumed));
    assert!(resumed.iter().any(|message| message.role == Role::User
        && message.content
            == vec![ContentBlock::Text {
                text: "ATOMIC-HISTORY".to_string()
            }]));
    assert!(resumed.iter().any(|message| message.role == Role::Assistant
        && message.content
            == vec![ContentBlock::Text {
                text: "old answer ".repeat(20)
            }]));
    assert!(resumed.iter().any(|message| message.role == Role::User
        && message.content
            == vec![ContentBlock::Text {
                text: "resume-after-failure".to_string()
            }]));

    gate.notify_one();
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    bridge.abort();
    storage.close();
}

#[tokio::test]
async fn raw_transcript_reconstructs_in_order() {
    // Given: byte-distinct transcript deltas persisted through the real bus bridge
    let temp = TempDir::new().expect("temporary directory");
    let config = storage_config(&temp);
    let storage = Storage::open(config.clone()).expect("storage opens");
    let bus = EventBus::new(32);
    let bridge = spawn_storage_bridge(&bus, storage.handle());
    let seeded = seed_audit_events(&bus);

    // When: the transcript is rebuilt solely from ordered stored events
    let events = wait_for_event_count(&config, 7).await;
    let rebuilt = events
        .iter()
        .filter_map(|stored| match &stored.event.kind {
            EventKind::Message(MessageEvent::MessageDelta { delta }) => Some(delta.clone()),
            EventKind::Lifecycle(_)
            | EventKind::Message(MessageEvent::ReasoningDelta { .. })
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_) => None,
        })
        .collect::<Vec<_>>();

    // Then: insertion order and every message byte are recoverable without a checkpoint projection
    assert_eq!(rebuilt, seeded);
    bridge.abort();
    storage.close();
}
