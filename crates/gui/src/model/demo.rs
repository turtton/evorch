use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event, EventBus,
    ProviderEvent,
};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use tokio::sync::Notify;

pub struct DemoScriptModel {
    bus: Arc<EventBus>,
    scripts: Mutex<HashMap<String, VecDeque<ChatResponse>>>,
    worker_message_sent: Notify,
    reviewer_message_sent: Notify,
    worker_reply_sent: Notify,
    reviewer_reply_sent: Notify,
    children_joined: AtomicBool,
}

impl DemoScriptModel {
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            bus,
            scripts: Mutex::new(HashMap::from([
                (
                    "DEMO-ORCH".to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-delegate-w1",
                            "delegate_background",
                            serde_json::json!({
                                "role": "worker",
                                "prompt": "DEMO-W1",
                                "name": "worker-w1"
                            }),
                        ),
                        tool_response(
                            "demo-delegate-r1",
                            "delegate_background",
                            serde_json::json!({
                                "role": "reviewer",
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
        let marker = initial_marker(messages)?;
        if marker == "DEMO-ORCH"
            && messages.iter().any(is_demo_review_result)
            && !self.children_joined.swap(true, Ordering::AcqRel)
        {
            self.worker_reply_sent.notified().await;
            self.reviewer_reply_sent.notified().await;
        }
        let (remaining_turns, response) = self.scripted_response(marker)?;
        let turn = match marker {
            "DEMO-ORCH" => 5 - remaining_turns,
            "DEMO-W1" | "DEMO-R1" => 2 - remaining_turns,
            _ => 1,
        };
        let model = self.selected_model(role);
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
            run_id: Some(invocation.run_id.clone()),
        }));
        Ok(response)
    }

    fn selected_model(&self, role: Role) -> String {
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
                ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
        })
        .ok_or_else(|| RuntimeError::Model {
            reason: "demo run did not contain an initial prompt".to_string(),
        })
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
