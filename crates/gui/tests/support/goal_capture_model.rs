use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentModel, Role, RuntimeError};

pub struct CaptureModel {
    pub messages: Arc<Mutex<Vec<Vec<Message>>>>,
    pub succeeds: bool,
    pub gate: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait]
impl AgentModel for CaptureModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture".into()
    }

    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        if invocation.run_id.starts_with("run-") {
            let first = {
                let mut requests = self.messages.lock().unwrap();
                requests.push(messages.to_vec());
                requests.len() == 1
            };
            if first && let Some(gate) = &self.gate {
                gate.notified().await;
            }
        }
        if self.succeeds {
            Ok(ChatResponse {
                message: Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::ContentBlock::Text {
                        text: "fixture reply".into(),
                    }],
                },
                usage: providers::Usage::default(),
                finish_reason: providers::FinishReason::Stop,
            })
        } else {
            Err(RuntimeError::Model {
                reason: "fixture failure".into(),
            })
        }
    }
}
