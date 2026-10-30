// RED was established before T1 by the runtime-side failing test
// delta_attribution::reasoning_block_emits_attributed_reasoning_delta_before_text.
// Commenting out the agent-loop Reasoning arm would make the exact run-entry
// assertion below fail equivalently; production code is not toggled here.

use std::sync::{Arc, Mutex, mpsc};

use async_trait::async_trait;
use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, EventBus};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::model::transcript::TranscriptEntry;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use tools::ToolExecutor;
use workspace_ui::UiSettings;

struct ScriptedModel {
    response: Mutex<Option<ChatResponse>>,
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.response
            .lock()
            .expect("script lock must not poison")
            .take()
            .ok_or_else(|| RuntimeError::Model {
                reason: "script exhausted".into(),
            })
    }

    fn selected_model(&self, role: Role, _category: Option<&str>) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

#[test]
fn reasoning_reaches_gui_transcripts_and_renders_when_agent_loop_streams() {
    // Given: a real runtime, event bus, pump, and workbench with one model reply.
    let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
    let bus = Arc::new(EventBus::new(32));
    let executor = Arc::new(ToolExecutor::new(bus.clone()));
    let model = Arc::new(ScriptedModel {
        response: Mutex::new(Some(ChatResponse {
            message: Message {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::Anthropic {
                        block: serde_json::json!({"type":"thinking","thinking":"weighing options","signature":"private-signature"}),
                        prefix_hash: None,
                    },
                    ContentBlock::Anthropic {
                        block: serde_json::json!({"type":"redacted_thinking","data":"private-encrypted-data"}),
                        prefix_hash: None,
                    },
                    ContentBlock::Text {
                        text: "final answer".into(),
                    },
                ],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })),
    });
    let runtime = AgentRuntime::new(bus.clone(), executor, model);
    let (repaint_tx, repaint_rx) = mpsc::channel();
    let pump = EventPump::spawn(
        rt.handle(),
        bus.subscribe(),
        Some(Arc::new(move || {
            let _ = repaint_tx.send(());
        })),
    );
    let state = WorkbenchState::new(runtime.clone(), &UiSettings::default())
        .expect("default state builds")
        .with_pump(pump);
    let mut harness = Harness::builder()
        .with_size(vec2(800.0, 600.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<AgentRuntime>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest());
            },
            state,
        );

    // When: a worker completes through the real agent loop (no injected events).
    let run_id = {
        let _guard = rt.enter();
        runtime.delegate_background(Role::Worker, "answer".into(), RunConfig::default())
    };
    let phase = rt
        .block_on(runtime.wait(run_id))
        .expect("worker run must exist");
    assert_eq!(phase, AgentRunPhase::Done);
    let run_id = run_id.to_string();

    // Then: both ordered blocks reach the run, neither reaches the thread,
    // and the run pane renders the actual event-folded transcript.
    let expected = [
        TranscriptEntry::UserMessage {
            text: "answer".into(),
            skills: Vec::new(),
        },
        TranscriptEntry::Reasoning {
            text: "weighing options".into(),
            run_id: Some(run_id.clone()),
        },
        TranscriptEntry::Message {
            text: "final answer".into(),
            run_id: Some(run_id.clone()),
        },
    ];
    loop {
        repaint_rx
            .recv()
            .expect("event pump remains alive until the transcript is rendered");
        harness.run_steps(4);
        if harness
            .state()
            .transcripts()
            .run(&run_id)
            .is_some_and(|transcript| transcript.entries() == expected)
        {
            break;
        }
    }
    let transcript = harness.state().transcripts().run(&run_id);
    assert_eq!(
        transcript.map(|transcript| transcript.entries()),
        Some(expected.as_slice()),
        "run transcript must receive reasoning before text"
    );
    assert!(expected.iter().all(|entry| {
        !harness
            .state()
            .transcripts()
            .thread()
            .entries()
            .contains(entry)
    }));
    let mut pane = Harness::new_ui(|ui| {
        gui::panes::agent_transcript::agent_transcript_pane(ui, &run_id, transcript);
    });
    pane.run();
    assert!(pane.query_by_label("You: answer").is_some());
    assert!(pane.query_by_label("thinking").is_some());
    assert!(pane.query_by_label("weighing options").is_none());
    pane.get_by_label("thinking").click();
    pane.run();
    assert!(pane.query_by_label("weighing options").is_some());
    assert!(pane.query_by_label("final answer").is_some());
}

#[test]
fn responses_native_state_never_reaches_transcripts_panes_or_recorded_events() {
    // Given: a real runtime whose reply carries only native Responses state and text,
    // with a subscriber feeding the recorder bridge exactly what the bus carried.
    const CIPHERTEXT: &str = "gAAAAB3-gui-recorder-sentinel-ciphertext";
    const ITEM_ID: &str = "rs_gui_recorder_sentinel";
    let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
    let bus = Arc::new(EventBus::new(256));
    let mut recorded = bus.subscribe();
    // Drain concurrently from the start so the recorder copy cannot lag; the only
    // run's transition to Done is the terminal signal.
    let collector = rt.spawn(async move {
        let mut events = Vec::new();
        loop {
            let event = recorded
                .recv()
                .await
                .expect("bus stays open until the terminal transition is observed");
            let terminal = match &event.kind {
                event_bus::EventKind::Lifecycle(
                    event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to, .. },
                ) if *to == AgentRunPhase::Done => Some(run_id.clone()),
                _ => None,
            };
            events.push(event);
            if let Some(run_id) = terminal {
                break (run_id, events);
            }
        }
    });
    let executor = Arc::new(ToolExecutor::new(bus.clone()));
    let model = Arc::new(ScriptedModel {
        response: Mutex::new(Some(ChatResponse {
            message: Message {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::ResponsesReasoning {
                        item: serde_json::json!({
                            "type": "reasoning",
                            "id": ITEM_ID,
                            "summary": [],
                            "encrypted_content": CIPHERTEXT
                        }),
                        origin: providers::ResolvedResponsesScope {
                            dialect: providers::ResponsesDialect::OpenAi,
                            profile: String::new(),
                            route_model: "gpt-5.1".into(),
                            wire_model: "gpt-5.1".into(),
                            speed: providers::ResponsesSpeed::Standard,
                            service_tier: None,
                            endpoint_fingerprint: "ab".repeat(32),
                            account: providers::ResponsesAccount::Unavailable,
                        },
                    },
                    ContentBlock::Text {
                        text: "final answer".into(),
                    },
                ],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })),
    });
    let runtime = AgentRuntime::new(bus.clone(), executor, model);
    let (repaint_tx, repaint_rx) = mpsc::channel();
    let pump = EventPump::spawn(
        rt.handle(),
        bus.subscribe(),
        Some(Arc::new(move || {
            let _ = repaint_tx.send(());
        })),
    );
    let state = WorkbenchState::new(runtime.clone(), &UiSettings::default())
        .expect("default state builds")
        .with_pump(pump);
    let mut harness = Harness::builder()
        .with_size(vec2(800.0, 600.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<AgentRuntime>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest());
            },
            state,
        );

    // When: a worker completes through the real agent loop.
    let run_id = {
        let _guard = rt.enter();
        runtime.delegate_background(Role::Worker, "answer".into(), RunConfig::default())
    };
    let phase = rt
        .block_on(runtime.wait(run_id))
        .expect("worker run must exist");
    assert_eq!(phase, AgentRunPhase::Done);
    let run_id = run_id.to_string();

    // Then: the run transcript has the answer but no reasoning entry at all.
    // Runtime notices are outside this contract and only checked for the sentinel.
    let expected = [
        TranscriptEntry::UserMessage {
            text: "answer".into(),
            skills: Vec::new(),
        },
        TranscriptEntry::Message {
            text: "final answer".into(),
            run_id: Some(run_id.clone()),
        },
    ];
    loop {
        repaint_rx
            .recv()
            .expect("event pump remains alive until the answer is rendered");
        harness.run_steps(4);
        if harness
            .state()
            .transcripts()
            .run(&run_id)
            .is_some_and(|transcript| transcript.entries().contains(&expected[1]))
        {
            break;
        }
    }
    let transcript = harness.state().transcripts().run(&run_id);
    let entries = transcript
        .map(|transcript| transcript.entries())
        .unwrap_or_default();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| {
                !matches!(
                    entry,
                    TranscriptEntry::Notice { .. } | TranscriptEntry::Diagnostic { .. }
                )
            })
            .cloned()
            .collect::<Vec<_>>(),
        expected
    );
    let rendered = format!("{entries:?}");
    assert!(!rendered.contains(CIPHERTEXT) && !rendered.contains(ITEM_ID));
    let mut pane = Harness::new_ui(|ui| {
        gui::panes::agent_transcript::agent_transcript_pane(ui, &run_id, transcript);
    });
    pane.run();
    assert!(pane.query_by_label("final answer").is_some());
    assert!(
        pane.query_by_label("thinking").is_none(),
        "native state must not create an empty thinking row"
    );

    // And: every bus event up to the run's terminal transition is persisted by the
    // recorder, and none of them carries the ciphertext or the native item id.
    let (terminal_run, events) = rt.block_on(collector).expect("collector task completes");
    assert_eq!(terminal_run, run_id);
    let dir = tempfile::tempdir().expect("temporary directory");
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..storage::StorageConfig::default()
    };
    let storage = storage::Storage::open(config.clone()).expect("storage opens");
    let mut bridge = recorder::storage_bridge::StorageBridge::new(storage.handle(), "session");
    for event in &events {
        bridge
            .handle_event(event)
            .expect("recorder persists the event");
    }
    bridge.flush_usage();
    let persisted: Vec<String> = storage::Database::open(&config)
        .expect("database opens")
        .events_all_ordered()
        .expect("events load")
        .iter()
        .map(|stored| serde_json::to_string(&stored.event).expect("event serializes"))
        .collect();
    assert!(
        persisted.iter().any(|event| event.contains("final answer")),
        "the recorder must have persisted the completed visible message"
    );
    for event in persisted.iter().chain(
        events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .iter(),
    ) {
        assert!(!event.contains(CIPHERTEXT), "event leaked the ciphertext");
        assert!(!event.contains(ITEM_ID), "event leaked the native item id");
    }
}
