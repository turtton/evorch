use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::sync::{Arc, Mutex};

use agents::Role;
use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, RecvError};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{AgentModel, AgentRuntime, RunConfig, RuntimeError};
use sandbox::BwrapConfig;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tools::ToolExecutor;

/// 外部プロバイダーを使わず、ロールごとの応答を決定的に返すデモ用モデルです。
struct ScriptedModel {
    scripts: Mutex<HashMap<&'static str, VecDeque<ChatResponse>>>,
    reviewer_waiting: Arc<Notify>,
}

impl ScriptedModel {
    fn new(reviewer_waiting: Arc<Notify>) -> Self {
        Self {
            scripts: Mutex::new(HashMap::from([
                (
                    "ORCH",
                    VecDeque::from([
                        response(
                            vec![
                                tool(
                                    "delegate-worker",
                                    "delegate_background",
                                    json!({ "role": "worker", "prompt": "W1" }),
                                ),
                                tool(
                                    "delegate-explorer",
                                    "delegate_background",
                                    json!({
                                        "role": "explorer",
                                        "prompt": "E1",
                                        "interactive": true
                                    }),
                                ),
                            ],
                            FinishReason::ToolUse,
                        ),
                        response(
                            vec![tool("wait-worker", "wait", json!({ "run_id": "run-2" }))],
                            FinishReason::ToolUse,
                        ),
                        response(
                            vec![tool(
                                "message-explorer",
                                "send_message",
                                json!({ "run_id": "run-3", "message": "調査を続けてください" }),
                            )],
                            FinishReason::ToolUse,
                        ),
                        response(
                            vec![tool(
                                "delegate-reviewer",
                                "delegate_background",
                                json!({
                                    "role": "reviewer",
                                    "prompt": "R1",
                                    "interactive": true
                                }),
                            )],
                            FinishReason::ToolUse,
                        ),
                        response(
                            vec![tool(
                                "cancel-reviewer",
                                "cancel",
                                json!({ "run_id": "run-4" }),
                            )],
                            FinishReason::ToolUse,
                        ),
                        response(
                            vec![tool(
                                "finish",
                                "finish",
                                json!({ "result": "demo complete" }),
                            )],
                            FinishReason::ToolUse,
                        ),
                    ]),
                ),
                (
                    "W1",
                    VecDeque::from([response(
                        vec![ContentBlock::Text {
                            text: "Worker: 実装を完了しました".to_string(),
                        }],
                        FinishReason::Stop,
                    )]),
                ),
                (
                    "E1",
                    VecDeque::from([
                        response(
                            vec![ContentBlock::Text {
                                text: "Explorer: 追加指示を待ちます".to_string(),
                            }],
                            FinishReason::Stop,
                        ),
                        response(
                            vec![ContentBlock::Text {
                                text: "Explorer: 調査を完了しました".to_string(),
                            }],
                            FinishReason::Stop,
                        ),
                    ]),
                ),
                (
                    "R1",
                    VecDeque::from([response(
                        vec![ContentBlock::Text {
                            text: "Reviewer: 入力を待ちます".to_string(),
                        }],
                        FinishReason::Stop,
                    )]),
                ),
            ])),
            reviewer_waiting,
        }
    }
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let marker = messages
            .first()
            .and_then(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    ContentBlock::Reasoning { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. } => None,
                })
            })
            .ok_or_else(|| RuntimeError::Model {
                reason: "demo run did not contain an initial prompt".to_string(),
            })?;
        let response = self
            .scripts
            .lock()
            .map_err(|_| RuntimeError::Model {
                reason: "demo script lock was poisoned".to_string(),
            })?
            .get_mut(marker)
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| RuntimeError::Model {
                reason: format!("demo script exhausted for {marker}"),
            })?;
        let cancels_reviewer =
            response.message.content.iter().any(
                |block| matches!(block, ContentBlock::ToolUse { name, .. } if name == "cancel"),
            );

        println!("[model] role={role:?} marker={marker}");
        if cancels_reviewer {
            self.reviewer_waiting.notified().await;
        }
        Ok(response)
    }

    fn selected_model(&self, _role: Role) -> String {
        "demo-script".to_string()
    }
}

fn tool(id: &str, name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input,
    }
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

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn Error>> {
    // EventBus を共有し、実際の ToolExecutor とモデル境界でランタイムを組み立てる。
    let bus = Arc::new(EventBus::new(64));
    let reviewer_waiting = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::new(Arc::clone(&reviewer_waiting)));
    let workspace_root = std::env::current_dir()?;
    let executor = Arc::new(ToolExecutor::with_production_sandbox(
        Arc::clone(&bus),
        BwrapConfig::new(workspace_root),
    )?);
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);

    let mut receiver = bus.subscribe();
    let printer = tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => match &event.kind {
                    EventKind::Lifecycle(event) => {
                        println!("[event] kind=Lifecycle payload={event:?}");
                        if matches!(
                            event,
                            LifecycleEvent::AgentRunStateChanged {
                                run_id,
                                to: AgentRunPhase::Waiting,
                                ..
                            } if run_id == "run-4"
                        ) {
                            reviewer_waiting.notify_one();
                        }
                        if matches!(
                            event,
                            LifecycleEvent::BackgroundTaskCompleted { task_id }
                                if task_id == "run-1"
                        ) {
                            return;
                        }
                    }
                    EventKind::Message(event) => println!("[event] kind=Message payload={event:?}"),
                    EventKind::Tool(event) => println!("[event] kind=Tool payload={event:?}"),
                    EventKind::Usage(event) => println!("[event] kind=Usage payload={event:?}"),
                    EventKind::Provider(event) => {
                        println!("[event] kind=Provider payload={event:?}")
                    }
                    EventKind::Fault(event) => println!("[event] kind=Fault payload={event:?}"),
                    EventKind::AgentMessage(event) => {
                        println!("[event] kind=AgentMessage payload={event:?}")
                    }
                },
                Err(RecvError::Lagged(skipped)) => {
                    println!("[event] kind=Lagged payload=skipped:{skipped}")
                }
                Err(RecvError::Closed) => return,
            }
        }
    });

    // Orchestrator が Worker と Explorer を並列起動し、待機・再開・キャンセル・完了を指示する。
    let orchestrator =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());
    let phase = runtime.wait(orchestrator).await?;
    println!("[summary] {orchestrator} finished with {phase:?}");
    printer.await?;
    Ok(())
}
