#[path = "support/codex.rs"]
mod codex_support;
#[path = "support/codex_contract.rs"]
mod contract_support;
#[path = "support/codex_priority_contract.rs"]
mod priority_contract;
mod support;

use std::sync::Arc;
use std::time::Duration;

use codex_support::{client, request};
use contract_support::{CodexBodyMatcher, CodexIdMatcher, seeded_store};
use event_bus::{EventBus, ProviderEvent, UsageEvent};
use futures_util::StreamExt;
use providers::provider::codex::tokens::InMemoryTokenStore;
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{
    ContentBlock, FinishReason, ProviderAuth, ProviderClient, ProviderError, StreamEvent, Usage,
};
use serde_json::json;
use support::{fixture, json_response, next_provider_event, next_usage_event, sse_response};
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, ResponseTemplate};

async fn mount(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer access-tok-1"))
        .and(header("chatgpt-account-id", "acc-123"))
        .and(header("originator", "codex_cli_rs"))
        .and(header(
            "user-agent",
            format!("codex_cli_rs/{}", providers::CODEX_INFERENCE_CLIENT_VERSION),
        ))
        .and(header("accept", "text/event-stream"))
        .and(CodexIdMatcher)
        .and(CodexBodyMatcher)
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn session_id_is_stable_across_requests_and_turn_id_fresh() {
    // Given: 同じクライアントからの2リクエストを記録するサーバー。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(sse_response(&fixture("codex", "responses_success.sse")))
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server, seeded_store());
    // When: 同一クライアントで連続した2ターンを送信する。
    for _ in 0..2 {
        client
            .send(&ProviderAuth::new(""), &request())
            .await
            .expect("send succeeds");
    }
    // Then: セッションは共通で、ターン識別子だけが更新される。
    let requests = server
        .received_requests()
        .await
        .expect("requests are recorded");
    assert!(
        requests
            .iter()
            .all(|request| CodexIdMatcher.matches(request))
    );
    let triples: Vec<_> = requests
        .iter()
        .map(|request| {
            ["session-id", "thread-id", "x-client-request-id"]
                .map(|name| request.headers[name].to_str().expect("UUID is ASCII"))
        })
        .collect();
    assert_eq!(triples[0][0], triples[1][0]);
    assert_ne!(triples[0][1], triples[1][1]);
    for [_, thread, request] in triples {
        assert_eq!(thread, request);
    }
}

#[tokio::test]
async fn send_sets_codex_headers_and_aggregates_stream() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("codex", "responses_success.sse")),
    )
    .await;

    let bus = Arc::new(EventBus::new(8));
    let mut rx = bus.subscribe();
    let response = CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            timeout: Duration::from_secs(1),
            event_bus: Some(bus),
        },
        seeded_store(),
    )
    .expect("Codex client can be built")
    .send(&ProviderAuth::new(""), &request())
    .await
    .expect("send succeeds");

    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "Hello world".into()
        }]
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 12,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0
        }
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert!(matches!(
        next_provider_event(&mut rx).await,
        ProviderEvent::RequestStarted { .. }
    ));
    assert!(matches!(
        next_usage_event(&mut rx).await,
        UsageEvent::Usage { .. }
    ));
    assert!(matches!(
        next_provider_event(&mut rx).await,
        ProviderEvent::RequestCompleted { .. }
    ));
}

#[tokio::test]
async fn stream_yields_canonical_events() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("codex", "responses_success.sse")),
    )
    .await;

    let events = client(&server, seeded_store())
        .stream(&ProviderAuth::new(""), &request())
        .await
        .expect("stream starts")
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events[0],
        Ok(StreamEvent::TextDelta {
            text: "Hello".into()
        })
    );
    assert_eq!(
        events[1],
        Ok(StreamEvent::TextDelta {
            text: " world".into()
        })
    );
    let StreamEvent::Completed { response } = events[2].as_ref().expect("completed event is valid")
    else {
        panic!("final event must be Completed")
    };
    assert_eq!(
        response.usage.input_tokens + response.usage.output_tokens,
        14
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

#[tokio::test]
async fn stream_tool_call_flow() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("codex", "responses_tool_call.sse")),
    )
    .await;
    let events = client(&server, seeded_store())
        .stream(&ProviderAuth::new(""), &request())
        .await
        .expect("stream starts")
        .collect::<Vec<_>>()
        .await;
    let StreamEvent::Completed { response } = events
        .last()
        .expect("events exist")
        .as_ref()
        .expect("event succeeds")
    else {
        panic!("final event must be Completed")
    };
    assert_eq!(
        response.message.content,
        vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "read_file".into(),
            input: json!({"path":"Cargo.toml"})
        }]
    );
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
}

#[tokio::test]
async fn send_maps_http_429_with_retry_after() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(429).insert_header("Retry-After", "2"),
    )
    .await;
    let error = client(&server, seeded_store())
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect_err("429 fails");
    assert_eq!(
        error,
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(2))
        }
    );
}

#[tokio::test]
async fn send_maps_http_500_to_http_error() {
    let server = MockServer::start().await;
    mount(&server, json_response(500, "boom")).await;
    let error = client(&server, seeded_store())
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect_err("500 fails");
    assert_eq!(
        error,
        ProviderError::Http {
            status: 500,
            body: "boom".into()
        }
    );
}

#[tokio::test]
async fn missing_token_bundle_fails_without_network() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let error = client(&server, Arc::new(InMemoryTokenStore::new()))
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect_err("missing token fails");
    assert!(
        matches!(error, ProviderError::Request(message) if message.contains("token bundle missing"))
    );
}
