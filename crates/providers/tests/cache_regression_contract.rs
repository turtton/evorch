use std::sync::Arc;
use std::time::Duration;

use event_bus::{DiagnosticSeverity, EventBus, EventKind};
use futures_util::StreamExt;
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{ChatRequest, ProviderAuth, ProviderClient};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request(run: &str) -> ChatRequest {
    serde_json::from_value(json!({
        "model": "cache-model",
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "a".repeat(400)}]},
            {"role": "user", "content": [{"type": "text", "text": "new"}]}
        ],
        "observation": {"run_id": run}
    }))
    .unwrap()
}

async fn mount_usage(server: &MockServer, cached: u64) {
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cache-response", "model": "cache-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 110, "completion_tokens": 1,
                "prompt_tokens_details": {"cached_tokens": cached}}
        })))
        .mount(server)
        .await;
}

async fn diagnostics_after(
    previous: Option<u64>,
    actual: u64,
    run: &str,
) -> Vec<event_bus::DiagnosticEvent> {
    let server = MockServer::start().await;
    let bus = Arc::new(EventBus::new(32));
    let client = OpenAiClient::new(OpenAiConfig {
        base_url: server.uri(),
        event_bus: Some(bus.clone()),
        ..OpenAiConfig::default()
    })
    .unwrap();
    let auth = ProviderAuth::new("test-key");
    // Given: optional completed cache-bearing request in this run.
    if let Some(cached) = previous {
        mount_usage(&server, cached).await;
        client.send(&auth, &request("warm-run")).await.unwrap();
    }
    let mut receiver = bus.subscribe();
    mount_usage(&server, actual).await;
    // When: the next real HTTP request completes.
    client.send(&auth, &request(run)).await.unwrap();
    let mut diagnostics = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Diagnostic(diagnostic) => diagnostics.push(diagnostic),
            EventKind::Provider(event_bus::ProviderEvent::RequestCompleted { .. }) => break,
            _ => {}
        }
    }
    diagnostics
}

#[tokio::test]
async fn cache_regression_fires_when_warm_ratio_drops() {
    let diagnostics = diagnostics_after(Some(100), 20, "warm-run").await;
    // Then: a correlated warning, not a failed provider request.
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "CacheRegression");
    assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Warning);
    assert_eq!(diagnostics[0].run_id.as_deref(), Some("warm-run"));
    assert!(
        diagnostics[0]
            .call_id
            .as_deref()
            .unwrap()
            .starts_with("req-")
    );
}

#[tokio::test]
async fn cache_regression_is_absent_when_ratio_is_at_threshold() {
    assert!(
        diagnostics_after(Some(100), 50, "warm-run")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn cache_regression_is_absent_when_first_request_is_cold() {
    assert!(diagnostics_after(None, 0, "warm-run").await.is_empty());
}

#[tokio::test]
async fn cache_regression_is_absent_when_previous_request_never_cached() {
    assert!(diagnostics_after(Some(0), 0, "warm-run").await.is_empty());
}

#[tokio::test]
async fn cache_regression_is_absent_when_run_changes() {
    assert!(
        diagnostics_after(Some(100), 0, "another-run")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn compatible_cache_key_is_absent_when_observation_is_set() {
    // Given: a compatible provider, even with the official provider label.
    let server = MockServer::start().await;
    let client =
        OpenAiCompatibleClient::new(server.uri(), "openai", Duration::from_secs(2), None).unwrap();
    mount_usage(&server, 0).await;
    // When: sending an observed request over HTTP.
    client
        .send(&ProviderAuth::new("test-key"), &request("run-key"))
        .await
        .unwrap();
    // Then: no unsupported field is sent.
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn compatible_cache_key_is_absent_when_streaming() {
    // Given: a compatible SSE endpoint and an observed request.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(include_str!("fixtures/openai/stream_text.sse")),
        )
        .mount(&server)
        .await;
    let client =
        OpenAiCompatibleClient::new(server.uri(), "local", Duration::from_secs(2), None).unwrap();
    // When: streaming through the real HTTP adapter.
    let mut stream = client
        .stream(&ProviderAuth::new("test-key"), &request("run-key"))
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    // Then: streaming also omits the unsupported field.
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("prompt_cache_key").is_none());
}
