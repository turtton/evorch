use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use event_bus::EventBus;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{
    ChatRequest, ContentBlock, HostedWebSearchRequest, Message, ObservationContext, Role, Usage,
};
use serde_json::{Value, json};
use wiremock::MockServer;

pub fn jwt(exp: u64, account: &str) -> String {
    let claims =
        json!({"exp": exp, "https://api.openai.com/auth": {"chatgpt_account_id": account}});
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

pub fn store(access: &str, account: &str, exp: u64) -> Arc<InMemoryTokenStore> {
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(&TokenBundle {
            access_token: access.into(),
            refresh_token: "refresh-old".into(),
            id_token: jwt(exp, account),
        })
        .unwrap();
    store
}

pub fn client(
    server: &MockServer,
    store: Arc<dyn CodexTokenStore>,
    event_bus: Option<Arc<EventBus>>,
) -> CodexClient {
    CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            event_bus,
            client_version: providers::CodexClientVersion::Fixed(providers::CodexCatalogVersion {
                version: "0.200.0".into(),
                warning: None,
            }),
            ..CodexConfig::default()
        },
        store,
    )
    .unwrap()
    .with_profile("selected-profile")
}

pub fn request() -> HostedWebSearchRequest {
    HostedWebSearchRequest {
        model: "selected-codex-model".into(),
        query: "latest Rust release".into(),
        max_results: None,
        reasoning_effort: Some("low".into()),
        observation: Some(ObservationContext {
            run_id: "search-run".into(),
            purpose: event_bus::RequestPurpose::WebSearch,
        }),
        usage_sink: None,
    }
}

pub fn chat_request() -> ChatRequest {
    ChatRequest {
        model: "selected-codex-model".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "ordinary chat".into(),
            }],
        }],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}

pub fn usage() -> Usage {
    Usage {
        input_tokens: 42,
        output_tokens: 7,
        cache_read_tokens: 30,
        cache_write_tokens: 0,
        reasoning_tokens: Some(3),
    }
}

pub fn search_call() -> Value {
    json!({"type":"web_search_call", "id":"ws-1", "status":"completed",
    "action":{"type":"search", "query":"latest Rust release", "sources":[
        {"type":"url", "url":"https://rust-lang.org/release"},
        {"type":"url", "url":"https://docs.rs", "title":"Rust docs"}
    ]}})
}

pub fn message() -> Value {
    json!({"type":"message", "id":"msg-1", "role":"assistant", "status":"completed", "content":[
        {"type":"output_text", "text":"Rust release details.", "annotations":[
            {"type":"url_citation", "url":"https://rust-lang.org/release", "title":"Rust release"},
            {"type":"url_citation", "url":"https://rust-lang.org/release", "title":"Repeated citation"}
        ]}
    ]})
}

pub fn completed(output: Value) -> Value {
    json!({"type":"response.completed", "response":{
        "id":"resp-search", "status":"completed", "output":output,
        "usage":{"input_tokens":42,"output_tokens":7,"input_tokens_details":{"cached_tokens":30},
            "output_tokens_details":{"reasoning_tokens":3}}
    }})
}

pub fn sse(events: impl IntoIterator<Item = Value>) -> String {
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

pub fn success() -> String {
    sse([completed(json!([search_call(), message()]))])
}
