// RED was established before T1 by the runtime-side failing test
// delta_attribution::reasoning_block_emits_attributed_reasoning_delta_before_text.
// Commenting out the agent-loop Reasoning arm would make the exact run-entry
// assertion below fail equivalently; production code is not toggled here.

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

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

    fn selected_model(&self, role: Role) -> String {
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
                    ContentBlock::Reasoning {
                        text: "weighing options".into(),
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
    let phase = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), runtime.wait(run_id))
            .await
            .expect("worker must finish within 5s")
            .expect("worker run must exist")
    });
    assert_eq!(phase, AgentRunPhase::Done);
    let run_id = run_id.to_string();

    // Then: both ordered blocks reach the run, reasoning reaches the thread,
    // and the run pane renders the actual event-folded transcript.
    let expected = [
        TranscriptEntry::Reasoning {
            text: "weighing options".into(),
        },
        TranscriptEntry::Message {
            text: "final answer".into(),
        },
    ];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = repaint_rx.recv_timeout(Duration::from_millis(200));
        harness.run();
        if harness
            .state()
            .transcripts()
            .run(&run_id)
            .is_some_and(|transcript| transcript.entries() == expected)
            || Instant::now() >= deadline
        {
            break;
        }
    }
    let transcript = harness.state().transcripts().run(&run_id);
    assert_eq!(
        transcript.map(|transcript| transcript.entries()),
        Some(expected.as_slice()),
        "run transcript must receive reasoning before text within 5s"
    );
    assert!(
        harness
            .state()
            .transcripts()
            .thread()
            .entries()
            .contains(&expected[0])
    );
    let mut pane = Harness::new_ui(|ui| {
        gui::panes::agent_transcript::agent_transcript_pane(ui, &run_id, transcript);
    });
    pane.run();
    assert!(pane.query_by_label("Reasoning: weighing options").is_some());
    assert!(pane.query_by_label("Message: final answer").is_some());
}
