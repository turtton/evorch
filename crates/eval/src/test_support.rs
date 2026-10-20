use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use event_bus::{Event, EventBus, ProviderEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};

#[derive(Debug, Clone)]
pub(crate) enum Step {
    Text(String),
    Tool {
        name: String,
        input: serde_json::Value,
    },
    /// Never answers, as a provider that hangs would.
    Hang,
}

pub(crate) fn text(text: &str) -> Step {
    Step::Text(text.into())
}

pub(crate) fn tool(name: &str, input: serde_json::Value) -> Step {
    Step::Tool {
        name: name.into(),
        input,
    }
}

/// Answers each request with the next scripted step, then with "done".
/// Every answer is reported on `bus` as a completed request using `tokens` tokens.
pub(crate) struct ScriptedModel {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Vec<Message>>>,
    bus: Mutex<Option<Arc<EventBus>>>,
    tokens: u64,
}

impl ScriptedModel {
    pub(crate) fn new(steps: impl IntoIterator<Item = Step>) -> Arc<Self> {
        Self::with_tokens(steps, 10)
    }

    pub(crate) fn with_tokens(steps: impl IntoIterator<Item = Step>, tokens: u64) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            bus: Mutex::new(None),
            tokens,
        })
    }

    pub(crate) fn attach(&self, bus: &Arc<EventBus>) {
        *self.bus.lock().unwrap() = Some(Arc::clone(bus));
    }

    pub(crate) fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }

    fn report(&self, invocation: &AgentInvocationContext) {
        if let Some(bus) = self.bus.lock().unwrap().as_ref() {
            bus.emit(Event::new(ProviderEvent::RequestCompleted {
                request_id: format!("request-{}", self.requests.lock().unwrap().len()),
                provider: "scripted".into(),
                profile: None,
                protocol: "test".into(),
                model: "scripted".into(),
                streaming: false,
                duration_ms: 1,
                input_tokens: self.tokens,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                finish_reason: "stop".into(),
                run_id: Some(invocation.run_id.clone()),
                reasoning_tokens: None,
                purpose: Some(invocation.purpose),
                service_tier: None,
            }));
        }
    }
}

#[async_trait::async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let step = self.steps.lock().unwrap().pop_front();
        let (content, finish_reason) = match step {
            Some(Step::Hang) => std::future::pending().await,
            Some(Step::Tool { name, input }) => (
                vec![ContentBlock::ToolUse {
                    id: format!("call-{}", self.requests.lock().unwrap().len()),
                    name,
                    input,
                }],
                FinishReason::ToolUse,
            ),
            Some(Step::Text(text)) => (vec![ContentBlock::Text { text }], FinishReason::Stop),
            None => (
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            ),
        };
        self.report(invocation);
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason,
            usage: Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "scripted".into()
    }
}

/// The simulated user's scripted decisions as the JSON the model would return.
pub(crate) fn decision(action: &str, text: &str) -> Step {
    Step::Text(
        serde_json::json!({"action": action, "text": text, "reason": "scripted"}).to_string(),
    )
}
