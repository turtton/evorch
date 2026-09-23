use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use egui::vec2;
use egui_kittest::Harness;
use event_bus::AgentRunPhase;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
};
use tools::ToolExecutor;
use workspace_ui::UiSettings;

use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::model::tasks::TaskRow;

#[path = "runtime_wiring/live.rs"]
mod live;

fn build_harness(
    state: WorkbenchState<AgentRuntime>,
) -> Harness<'static, WorkbenchState<AgentRuntime>> {
    Harness::builder()
        .with_size(vec2(800.0, 600.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<AgentRuntime>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest());
            },
            state,
        )
}

/// Prompt-marker keyed scripted model: "ORCH" delegates a worker then finishes,
/// "W1" answers with text and stops.
struct ScriptedModel {
    scripts: Mutex<HashMap<String, VecDeque<ChatResponse>>>,
    events: tokio::sync::Mutex<event_bus::EventReceiver>,
}

impl ScriptedModel {
    fn new(bus: &event_bus::EventBus) -> Self {
        Self {
            events: tokio::sync::Mutex::new(bus.subscribe()),
            scripts: Mutex::new(HashMap::from([
                (
                    "ORCH".to_string(),
                    VecDeque::from([
                        tool_response(
                            "delegate-worker",
                            "delegate",
                            serde_json::json!({
                                "background": true,
                                "role": "worker",
                                "prompt": "W1",
                                "name": "worker-w1"
                            }),
                        ),
                        tool_response(
                            "finish-orchestrator",
                            "finish",
                            serde_json::json!({ "result": "orchestrator finished" }),
                        ),
                    ]),
                ),
                (
                    "W1".to_string(),
                    VecDeque::from([text_response("worker finished", FinishReason::Stop)]),
                ),
            ])),
        }
    }
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let marker = messages
            .iter()
            .find(|message| message.role == MessageRole::User)
            .and_then(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.clone()),
                    ContentBlock::Image { .. }
                    | ContentBlock::Reasoning { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. } => None,
                })
            });
        // Exercise child completion before the parent's final response, rather
        // than relying on which Tokio worker wins the completion race.
        let final_orchestrator_turn = marker.as_deref() == Some("ORCH")
            && self.scripts.lock().expect("script lock")["ORCH"].len() == 1;
        if final_orchestrator_turn {
            let mut events = self.events.lock().await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let event = match events.recv().await {
                        Ok(event) => event,
                        // Startup/progress may exceed this observer's queue before
                        // the second model turn. The completion remains queued.
                        Err(event_bus::RecvError::Lagged(_)) => continue,
                        Err(event_bus::RecvError::Closed) => {
                            return Err(RuntimeError::Model {
                                reason: "completion relay event stream closed".into(),
                            });
                        }
                    };
                    if matches!(
                        event.kind,
                        event_bus::EventKind::AgentMessage(
                            event_bus::AgentMessageEvent::Delivered { .. }
                        )
                    ) {
                        return Ok(());
                    }
                }
            })
            .await
            .map_err(|_| RuntimeError::Model {
                reason: "completion relay event deadline".into(),
            })??;
        }
        let mut scripts = self.scripts.lock().expect("script lock must not poison");
        scripts
            .get_mut(marker.as_deref().unwrap_or_default())
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| RuntimeError::Model {
                reason: format!("script exhausted for {marker:?}"),
            })
    }

    fn selected_model(&self, role: Role, _category: Option<&str>) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

fn text_response(text: &str, finish_reason: FinishReason) -> ChatResponse {
    response(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        finish_reason,
    )
}

fn tool_response(id: &str, name: &str, input: serde_json::Value) -> ChatResponse {
    response(
        vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }],
        FinishReason::ToolUse,
    )
}

fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content,
        },
        usage: Usage::default(),
        finish_reason,
    }
}

#[test]
fn runtime_wiring_shows_orchestrator_and_delegated_worker_in_tasks() {
    // Given: a real AgentRuntime wired to a scripted model, an event pump, and
    // a workbench state backed by that runtime
    let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
    let bus = Arc::new(event_bus::EventBus::new(16));
    let executor = Arc::new(ToolExecutor::new(bus.clone()));
    let model = Arc::new(ScriptedModel::new(&bus));
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
    let mut harness = build_harness(state);

    // When: the orchestrator run delegates a background worker and both runs finish
    let run_id = {
        let _guard = rt.enter();
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default())
    };
    let phase = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), runtime.wait(run_id))
            .await
            .expect("orchestrator completion deadline")
    });
    assert_eq!(
        phase.expect("orchestrator run must finish"),
        AgentRunPhase::Done
    );

    // Then: the tasks rows converge to the delegated child run only
    let expected = vec![TaskRow {
        run_id: RunId::new(2),
        name: "worker-w1".into(),
        role: "Worker".into(),
        status: AgentRunPhase::Done,
        model: "test-worker".into(),
    }];
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.state().tasks().rows() != expected.as_slice() {
        repaint_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("runtime task rows must converge before the repaint deadline");
        harness.run_steps(4);
    }
    assert_eq!(harness.state().tasks().rows(), expected.as_slice());
}

#[tokio::test]
async fn completion_barrier_survives_a_lagged_observer() {
    let bus = event_bus::EventBus::new(16);
    let model = ScriptedModel::new(&bus);
    let messages = [Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "ORCH".into(),
        }],
    }];
    let invocation = AgentInvocationContext::default();
    model
        .complete(&invocation, Role::Orchestrator, &messages, &[])
        .await
        .unwrap();
    // Deterministically reproduce a delayed second turn after the child's
    // startup/progress fills the ring. Completion itself is still retained.
    for _ in 0..17 {
        bus.emit(event_bus::Event::new(
            event_bus::MessageEvent::MessageDelta {
                run_id: Some("run-2".into()),
                delta: "progress".into(),
            },
        ));
    }
    bus.emit(event_bus::Event::new(
        event_bus::AgentMessageEvent::Delivered {
            message: event_bus::AgentMessage {
                message_id: "completion".into(),
                sender_run_id: "run-2".into(),
                recipient_run_id: "run-1".into(),
                kind: event_bus::AgentMessageKind::Send,
                content: "worker finished".into(),
                reply_to: None,
            },
            disposition: event_bus::DeliveryDisposition::Aside,
        },
    ));
    let response = model
        .complete(&invocation, Role::Orchestrator, &messages, &[])
        .await
        .unwrap();
    assert!(
        matches!(&response.message.content[0], ContentBlock::ToolUse { name, .. } if name == "finish")
    );
}
