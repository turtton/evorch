use std::sync::Arc;
use std::time::Duration;

use providers::provider::codex::tokens::CodexTokenStore;
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{ChatRequest, ContentBlock, Message, Role};
use wiremock::MockServer;

pub const MODEL: &str = "gpt-5.1-codex";

pub fn request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(123),
        observation: None,
    }
}

pub fn client(server: &MockServer, store: Arc<dyn CodexTokenStore>) -> CodexClient {
    CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            timeout: Duration::from_secs(1),
            event_bus: None,
        },
        store,
    )
    .expect("Codex client can be built")
}
