mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{EventBus, UsageEvent};
use futures_util::StreamExt;
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient, ProviderError,
    Role, StreamEvent,
};
use support::{fixture, json_response, next_usage_event, sse_response};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

const MODEL: &str = "compatible-model";

fn request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".into(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
    }
}

// Given: カスタムbase URLとprovider label / When: send / Then: /chat/completionsへ送信しカスタムlabelのusageを発行する
#[tokio::test(flavor = "multi_thread")]
async fn send_uses_custom_base_url_and_provider_label() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_response(200, &fixture("openai", "send_text.json")))
        .expect(1)
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let response = client
        .send(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("send は成功する");

    assert_eq!(response.finish_reason, FinishReason::Stop);
    match next_usage_event(&mut receiver).await {
        UsageEvent::Usage {
            provider, model, ..
        } => {
            assert_eq!(provider, "test-compatible");
            assert_eq!(model, MODEL);
        }
        UsageEvent::CacheStats { .. } => panic!("Usage イベントを期待しました"),
    }
}

// Given: OpenAI互換SSE / When: stream / Then: text差分とCompletedを返す
#[tokio::test(flavor = "multi_thread")]
async fn stream_uses_openai_wire_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&fixture("openai", "stream_text.sse")))
        .mount(&server)
        .await;
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        None,
    )
    .expect("互換clientを構築できる");

    let events = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("streamを開始できる")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("streamは成功する");

    assert!(matches!(events.first(), Some(StreamEvent::TextDelta { text }) if text == "Hello"));
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { response }) if response.finish_reason == FinishReason::Stop)
    );
}

// Given: OpenAI互換backendの429 / When: send / Then: RateLimitedへ変換する
#[tokio::test(flavor = "multi_thread")]
async fn status_429_maps_to_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_response(429, r#"{"error":"limited"}"#))
        .mount(&server)
        .await;
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        None,
    )
    .expect("互換clientを構築できる");

    let error = client
        .send(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect_err("429になる");

    assert_eq!(error, ProviderError::RateLimited { retry_after: None });
}
