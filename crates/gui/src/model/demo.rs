use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event, EventBus,
    EventKind, EventReceiver, OrchestratorEvent, ProviderEvent, RecvError,
};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use tokio::sync::Notify;

const DEMO_GOAL_KEY: &str = "DEMO-GOAL";

pub struct DemoScriptModel {
    bus: Arc<EventBus>,
    scripts: Mutex<HashMap<String, VecDeque<ChatResponse>>>,
    worker_message_sent: Notify,
    reviewer_message_sent: Notify,
    worker_reply_sent: Notify,
    reviewer_reply_sent: Notify,
    children_joined: AtomicBool,
    // 構築時に subscribe しておき、最初の待機で drain task を起動して全イベントを
    // inbox に蓄積する。待機は inbox の replay から始めるため、対象イベントが
    // 待機より前に発行済みでも取り逃さない。
    inbox: Arc<Mutex<Vec<Event>>>,
    receiver: Mutex<Option<EventReceiver>>,
    inbox_notify: Arc<Notify>,
    drain_started: AtomicBool,
}

impl DemoScriptModel {
    pub fn new(bus: Arc<EventBus>) -> Self {
        let event_receiver = bus.subscribe();
        Self {
            bus,
            scripts: Mutex::new(HashMap::from([
                (
                    "DEMO-ORCH".to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-delegate-w1",
                            "delegate",
                            serde_json::json!({"target": {"role": "worker"},
                                "background": true,
                                "prompt": "DEMO-W1",
                                "name": "worker-w1"
                            }),
                        ),
                        tool_response(
                            "demo-delegate-r1",
                            "delegate",
                            serde_json::json!({"target": {"role": "reviewer"},
                                "background": true,
                                "prompt": "DEMO-R1",
                                "name": "reviewer-r1"
                            }),
                        ),
                        tool_response(
                            "demo-message-w1",
                            "send_message",
                            serde_json::json!({
                                "run_id": "run-2",
                                "message": "implement the goal"
                            }),
                        ),
                        tool_response(
                            "demo-message-r1",
                            "send_message",
                            serde_json::json!({
                                "run_id": "run-3",
                                "message": "review run-2"
                            }),
                        ),
                        text_response("demo complete"),
                    ]),
                ),
                (
                    "DEMO-W1".to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-worker-done",
                            "send",
                            serde_json::json!({
                                "run_id": "run-1",
                                "message": "worker done"
                            }),
                        ),
                        text_response("worker done"),
                    ]),
                ),
                (
                    "DEMO-R1".to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-review-lgtm",
                            "send",
                            serde_json::json!({
                                "run_id": "run-1",
                                "message": "LGTM"
                            }),
                        ),
                        text_response("review done"),
                    ]),
                ),
            ])),
            worker_message_sent: Notify::new(),
            reviewer_message_sent: Notify::new(),
            worker_reply_sent: Notify::new(),
            reviewer_reply_sent: Notify::new(),
            children_joined: AtomicBool::new(false),
            inbox: Arc::new(Mutex::new(Vec::new())),
            receiver: Mutex::new(Some(event_receiver)),
            inbox_notify: Arc::new(Notify::new()),
            drain_started: AtomicBool::new(false),
        }
    }

    fn scripted_response(&self, marker: &str) -> Result<(u64, ChatResponse), RuntimeError> {
        let mut scripts = self.scripts.lock().map_err(|_| RuntimeError::Model {
            reason: "demo script lock was poisoned".to_string(),
        })?;
        let script = scripts.get_mut(marker).ok_or_else(|| RuntimeError::Model {
            reason: format!("unknown demo script marker {marker}"),
        })?;
        let response = script
            .pop_front()
            .or_else(|| (marker == "DEMO-ORCH").then(|| text_response("demo complete")));
        let response = response.ok_or_else(|| RuntimeError::Model {
            reason: format!("demo script exhausted for {marker}"),
        })?;
        Ok((script.len() as u64, response))
    }

    /// 構築時に確保した receiver を drain する task を一度だけ起動する。
    /// receiver は構築時点以降のイベントを broadcast buffer に保持するため、
    /// drain 開始が遅れても取り逃さない (capacity 超過時は Lagged で流す)。
    fn ensure_drain(&self) {
        if self.drain_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(mut receiver) = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let inbox = Arc::clone(&self.inbox);
        let notify = Arc::clone(&self.inbox_notify);
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => {
                        inbox
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(event);
                        notify.notify_waiters();
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                }
            }
        });
    }

    /// root run に結び付いた thread goal が inbox に現れるまで待つ。
    async fn thread_goal_for_run(&self, run: &str) -> event_bus::ThreadGoalSnapshot {
        self.ensure_drain();
        loop {
            let notified = self.inbox_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let inbox = self
                    .inbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for event in inbox.iter().rev() {
                    if let EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated {
                        snapshot,
                    }) = &event.kind
                        && snapshot.root_run_id == run
                    {
                        return snapshot.clone();
                    }
                }
            }
            notified.await;
        }
    }

    fn generic_goal_response(
        snapshot: &event_bus::ThreadGoalSnapshot,
        messages: &[Message],
        review: bool,
    ) -> ChatResponse {
        let latest = messages
            .last()
            .and_then(|message| {
                message.content.iter().find_map(|block| {
                    if let ContentBlock::Text { text } = block {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_default();
        if review || latest.starts_with("[goal self-check]") {
            let epoch = if review {
                snapshot.epoch
            } else {
                latest
                    .split("with epoch ")
                    .nth(1)
                    .and_then(|part| part.split_whitespace().next())
                    .and_then(|epoch| epoch.parse().ok())
                    .unwrap_or(snapshot.epoch)
            };
            let checks = snapshot.criteria.iter().enumerate().map(|(criterion, _)| serde_json::json!({ "criterion": criterion, "met": true, "evidence": "The demo result is recorded in this conversation." })).collect::<Vec<_>>();
            let mut input = serde_json::json!({"epoch": epoch, "checks": checks});
            if review {
                input["findings"] = serde_json::json!([]);
            }
            tool_response(
                "demo-generic-goal-check",
                if review {
                    "submit_goal_review"
                } else {
                    "submit_goal_check"
                },
                input,
            )
        } else {
            text_response(
                "The demo objective is fulfilled. The result is recorded in this conversation.",
            )
        }
    }
}

#[async_trait]
impl AgentModel for DemoScriptModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let initial = initial_marker(messages)?;
        let marker = script_key(initial);
        let review_snapshot = if role == Role::Reviewer {
            initial
                .split("GOAL SNAPSHOT:\n")
                .nth(1)
                .and_then(|part| part.split("\nRECENT WORK").next())
                .and_then(|json| serde_json::from_str::<event_bus::ThreadGoalSnapshot>(json).ok())
        } else {
            None
        };
        let generic = if let Some(snapshot) = review_snapshot {
            Some(snapshot)
        } else if marker == DEMO_GOAL_KEY {
            Some(self.thread_goal_for_run(&invocation.run_id).await)
        } else {
            None
        };
        let (remaining_turns, response) = if let Some(snapshot) = generic {
            (
                0,
                Self::generic_goal_response(&snapshot, messages, role == Role::Reviewer),
            )
        } else {
            if marker == "DEMO-ORCH"
                && messages.iter().any(is_demo_review_result)
                && !self.children_joined.swap(true, Ordering::AcqRel)
            {
                self.worker_reply_sent.notified().await;
                self.reviewer_reply_sent.notified().await;
            }
            self.scripted_response(marker)?
        };
        let turn = match marker {
            "DEMO-ORCH" => 5 - remaining_turns,
            "DEMO-W1" | "DEMO-R1" => 2 - remaining_turns,
            _ => 1,
        };
        let model = self.selected_model(role, invocation.category.as_deref());
        let request_id = format!("demo-{}-{turn}", invocation.run_id);
        self.bus.emit(Event::new(ProviderEvent::RequestStarted {
            request_id: request_id.clone(),
            provider: "demo".to_string(),
            profile: None,
            protocol: "scripted".to_string(),
            model: model.clone(),
            streaming: false,
            run_id: Some(invocation.run_id.clone()),
        }));
        match (marker, remaining_turns) {
            ("DEMO-ORCH", 2) => self.worker_message_sent.notify_one(),
            ("DEMO-ORCH", 1) => {
                self.reviewer_message_sent.notify_one();
            }
            ("DEMO-W1", 1) => {
                self.worker_message_sent.notified().await;
            }
            ("DEMO-W1", 0) => self.worker_reply_sent.notify_one(),
            ("DEMO-R1", 1) => {
                self.reviewer_message_sent.notified().await;
            }
            ("DEMO-R1", 0) => {
                self.bus.emit(Event::new(AgentMessageEvent::Delivered {
                    message: AgentMessage {
                        message_id: "demo-review-message".to_string(),
                        sender_run_id: invocation.run_id.clone(),
                        recipient_run_id: "run-1".to_string(),
                        kind: AgentMessageKind::Send,
                        content: "LGTM".to_string(),
                        reply_to: None,
                    },
                    disposition: DeliveryDisposition::Aside,
                }));
                self.reviewer_reply_sent.notify_one();
            }
            _ => {}
        }
        self.bus.emit(Event::new(ProviderEvent::RequestCompleted {
            request_id,
            provider: "demo".to_string(),
            profile: None,
            protocol: "scripted".to_string(),
            model,
            streaming: false,
            duration_ms: 0,
            input_tokens: 120 * turn,
            output_tokens: 40 * turn,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: finish_reason_name(&response.finish_reason).to_string(),
            service_tier: None,
            run_id: Some(invocation.run_id.clone()),
            purpose: None,
            reasoning_tokens: None,
        }));
        Ok(response)
    }

    fn selected_model(&self, role: Role, _category: Option<&str>) -> String {
        format!("demo-{}", role.name().to_lowercase())
    }
}

fn is_demo_review_result(message: &Message) -> bool {
    message.content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolResult { tool_call_id, .. }
                if tool_call_id == "demo-message-r1"
        )
    })
}

fn initial_marker(messages: &[Message]) -> Result<&str, RuntimeError> {
    messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Compaction { .. } => {
                    tracing::warn!("この処理では compaction block をスキップします");
                    None
                }
                ContentBlock::Image { .. }
                | ContentBlock::Reasoning { .. }
                | ContentBlock::Anthropic { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
        })
        .ok_or_else(|| RuntimeError::Model {
            reason: "demo run did not contain an initial prompt".to_string(),
        })
}

/// 最初の user プロンプトを script key へ解決する。
///
/// 3 つの会話 script は全文一致、thread goal 用の key は先頭行の部分一致で
/// 解決する (`/goal` の entry prompt は利用者の本文を含むため)。
/// 未知のプロンプトは全文をそのまま返し、unknown marker エラーになる。
fn script_key(prompt: &str) -> &str {
    if matches!(prompt, "DEMO-ORCH" | "DEMO-W1" | "DEMO-R1") {
        return prompt;
    }
    let first_line = prompt.lines().next().unwrap_or(prompt);
    if first_line.contains(DEMO_GOAL_KEY) {
        return DEMO_GOAL_KEY;
    }
    prompt
}

fn text_response(text: &str) -> ChatResponse {
    response(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        FinishReason::Stop,
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

const fn finish_reason_name(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolUse => "tool_use",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Other(_) => "other",
    }
}
