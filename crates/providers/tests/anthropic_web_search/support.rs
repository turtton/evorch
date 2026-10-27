use std::sync::Arc;

use event_bus::EventBus;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::provider::claude::{
    ClaudeClient, ClaudeOAuthConfig, ClaudeTokenBundle, ClaudeTokenStore, InMemoryClaudeTokenStore,
};
use providers::{ChatRequest, HostedWebSearchRequest, ObservationContext, ProviderClient, Usage};
use serde_json::{Value, json};
use wiremock::MockServer;

pub fn bundle(account: &str, token: &str, expires_at: u64) -> ClaudeTokenBundle {
    ClaudeTokenBundle {
        access_token: token.into(),
        refresh_token: "secret-refresh".into(),
        expires_at,
        account_id: Some(account.into()),
        email: None,
        org_id: None,
        org_name: None,
    }
}

pub fn store() -> Arc<InMemoryClaudeTokenStore> {
    let store = Arc::new(InMemoryClaudeTokenStore::new());
    store
        .save(&bundle(
            "secret-account",
            "secret-subscription-token",
            u64::MAX,
        ))
        .unwrap();
    store
}

pub fn claude(
    server: &MockServer,
    store: Arc<InMemoryClaudeTokenStore>,
    event_bus: Option<Arc<EventBus>>,
) -> ClaudeClient {
    ClaudeClient::new(
        AnthropicConfig {
            base_url: server.uri(),
            event_bus,
            ..Default::default()
        },
        store,
    )
    .unwrap()
    .with_profile("selected-profile")
    .with_oauth_config(ClaudeOAuthConfig {
        token_url: format!("{}/token", server.uri()),
        ..Default::default()
    })
    .unwrap()
}

pub fn client(
    server: &MockServer,
    oauth: bool,
    event_bus: Option<Arc<EventBus>>,
) -> Box<dyn ProviderClient> {
    if oauth {
        Box::new(claude(server, store(), event_bus))
    } else {
        Box::new(
            AnthropicClient::new(AnthropicConfig {
                base_url: server.uri(),
                event_bus,
                ..Default::default()
            })
            .unwrap()
            .with_profile("selected-profile"),
        )
    }
}

pub fn request() -> HostedWebSearchRequest {
    HostedWebSearchRequest {
        model: "claude-sonnet-4-6".into(),
        query: "latest Rust release".into(),
        max_results: None,
        reasoning_effort: None,
        observation: Some(ObservationContext {
            run_id: "search-run".into(),
            ..Default::default()
        }),
        usage_sink: None,
    }
}

pub fn chat_request() -> ChatRequest {
    let mut chat: ChatRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4-6", "messages":[{"role":"user","content":[{"type":"text","text":"ordinary chat"}]}]
    })).unwrap();
    chat.observation = request().observation;
    chat
}

pub fn usage() -> Usage {
    Usage {
        context_tokens: None,
        input_tokens: 42,
        output_tokens: 7,
        cache_read_tokens: 30,
        cache_write_tokens: 2,
        reasoning_tokens: None,
    }
}

pub fn call(id: &str) -> Value {
    json!({"type":"server_tool_use", "id":id, "name":"web_search", "input":{"query":"latest Rust release"}})
}

pub fn results(id: &str) -> Value {
    json!({"type":"web_search_tool_result", "tool_use_id":id, "content":[
        {"type":"web_search_result","url":"https://rust-lang.org/release", "title":"Search title", "encrypted_content":"opaque-search-private"},
        {"type":"web_search_result","url":"https://docs.rs", "title":"Rust docs", "encrypted_content":"opaque-docs-private"}
    ]})
}

pub fn message() -> Value {
    json!({"type":"text", "text":"Rust release details.", "citations":[
        {"type":"web_search_result_location", "url":"https://rust-lang.org/release", "title":"Rust release", "encrypted_index":"opaque-index-private", "cited_text":"release"},
        {"type":"web_search_result_location", "url":"https://rust-lang.org/release", "title":"Duplicate", "encrypted_index":"opaque-index-private", "cited_text":"release"}
    ]})
}

pub fn completed(content: Value) -> Value {
    json!({"id":"msg_search", "type":"message", "role":"assistant", "content":content, "stop_reason":"end_turn",
        "usage":{"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":30,"cache_creation_input_tokens":2}})
}

pub fn success() -> Value {
    completed(json!([call("srv_one"), results("srv_one"), message()]))
}
