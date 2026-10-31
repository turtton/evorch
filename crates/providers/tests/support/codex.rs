use std::sync::Arc;
use std::time::Duration;

use providers::provider::codex::tokens::CodexTokenStore;
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{ChatRequest, ContentBlock, Message, Role};
use wiremock::MockServer;

pub const MODEL: &str = "gpt-5.1-codex";

pub fn request() -> ChatRequest {
    ChatRequest {
        responses_replay: None,
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
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
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

/// Serves an empty Codex catalog. A Codex client reads `GET {base}/models` once before its
/// first generation, so a server that scripts a chat flow must answer it.
#[allow(dead_code)]
pub async fn mount_empty_catalog(server: &MockServer) {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("GET"))
        .and(path_regex("/models$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
        .mount(server)
        .await;
}

/// The requests that carried a Responses body (generation or compaction), in arrival
/// order, without the catalog read.
#[allow(dead_code)]
pub async fn response_requests(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .expect("requests are recorded")
        .into_iter()
        .filter(|request| request.method == wiremock::http::Method::POST)
        .collect()
}
