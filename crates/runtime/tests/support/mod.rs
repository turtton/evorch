#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use agents::Role;
use async_trait::async_trait;
use event_bus::{Event, EventReceiver};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{AgentModel, RuntimeError};
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, timeout};

pub struct ScriptedModel {
    script: Mutex<VecDeque<Result<ChatResponse, RuntimeError>>>,
    keyed: Mutex<HashMap<String, VecDeque<Result<ChatResponse, RuntimeError>>>>,
    observed: Mutex<Vec<Vec<Message>>>,
    gate: Option<Arc<Notify>>,
}

impl ScriptedModel {
    pub fn new(script: impl IntoIterator<Item = Result<ChatResponse, RuntimeError>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            keyed: Mutex::new(HashMap::new()),
            observed: Mutex::new(Vec::new()),
            gate: None,
        }
    }

    pub fn gated(
        script: impl IntoIterator<Item = Result<ChatResponse, RuntimeError>>,
        gate: Arc<Notify>,
    ) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            keyed: Mutex::new(HashMap::new()),
            observed: Mutex::new(Vec::new()),
            gate: Some(gate),
        }
    }

    pub async fn add_keyed(
        &self,
        marker: &str,
        script: impl IntoIterator<Item = Result<ChatResponse, RuntimeError>>,
    ) {
        self.keyed
            .lock()
            .await
            .insert(marker.to_string(), script.into_iter().collect());
    }

    pub async fn observed(&self) -> Vec<Vec<Message>> {
        self.observed.lock().await.clone()
    }
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.observed.lock().await.push(messages.to_vec());
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }

        let marker = messages.first().and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
        });
        if let Some(marker) = marker {
            let mut keyed = self.keyed.lock().await;
            if let Some(script) = keyed.get_mut(marker) {
                return script.pop_front().unwrap_or_else(|| {
                    Err(RuntimeError::Model {
                        reason: format!("script exhausted for {marker}"),
                    })
                });
            }
        }

        self.script.lock().await.pop_front().unwrap_or_else(|| {
            Err(RuntimeError::Model {
                reason: "script exhausted".to_string(),
            })
        })
    }
}

pub fn text_response(text: &str, finish_reason: FinishReason) -> ChatResponse {
    response(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        finish_reason,
    )
}

pub fn tool_response(id: &str, name: &str, input: serde_json::Value) -> ChatResponse {
    response(
        vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }],
        FinishReason::ToolUse,
    )
}

pub fn tool_responses(
    uses: impl IntoIterator<Item = (&'static str, &'static str, serde_json::Value)>,
) -> ChatResponse {
    response(
        uses.into_iter()
            .map(|(id, name, input)| ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            })
            .collect(),
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

pub async fn collect_events(receiver: &mut EventReceiver, count: usize) -> Vec<Event> {
    let mut events = Vec::with_capacity(count);
    while events.len() < count {
        let event = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("event timeout")
            .expect("event receiver remains open");
        events.push(event);
    }
    events
}
