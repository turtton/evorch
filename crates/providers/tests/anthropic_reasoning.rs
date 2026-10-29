//! API-key Anthropic requests carry the selected reasoning effort only when the model supports it.
#[allow(dead_code)]
mod support;

use futures_util::StreamExt;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::{
    ChatRequest, ContentBlock, Message, ProviderAuth, ProviderClient, ProviderError, Role,
};
use serde_json::{Value, json};
use support::{fixture, json_response, sse_response};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer};

fn request(model: &str, effort: Option<&str>) -> ChatRequest {
    ChatRequest {
        responses_replay: None,
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(8192),
        reasoning_effort: effort.map(str::to_owned),
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}

async fn client(server: &MockServer) -> AnthropicClient {
    AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap()
}

/// Sends one non-streaming request and returns the captured wire body and headers.
async fn send(request: &ChatRequest) -> Result<(Value, bool), ProviderError> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(json_response(200, &fixture("anthropic", "send_text.json")))
        .mount(&server)
        .await;
    client(&server)
        .await
        .send(&ProviderAuth::new("sk-ant-test"), request)
        .await?;
    let captured = server.received_requests().await.unwrap();
    assert_eq!(captured.len(), 1);
    Ok((
        serde_json::from_slice(&captured[0].body).unwrap(),
        captured[0].headers.contains_key("anthropic-beta"),
    ))
}

// Given: API-key models that support effort / When: an explicit effort is selected
// Then: the wire carries the thinking mode and effort, without the Claude Code betas.
#[tokio::test]
async fn supported_effort_is_sent_without_beta_headers() {
    for (model, effort, thinking, output_config) in [
        (
            "claude-sonnet-4-6",
            "high",
            json!({"type": "adaptive", "display": "summarized"}),
            Some(json!({"effort": "high"})),
        ),
        (
            "claude-opus-4-7",
            "xhigh",
            json!({"type": "adaptive", "display": "summarized"}),
            Some(json!({"effort": "xhigh"})),
        ),
        (
            "claude-opus-4-5-20251101",
            "medium",
            json!({"type": "enabled", "budget_tokens": 4096}),
            Some(json!({"effort": "medium"})),
        ),
        (
            "claude-sonnet-4-5",
            "low",
            json!({"type": "enabled", "budget_tokens": 1024}),
            None,
        ),
        (
            "claude-sonnet-4-6",
            "none",
            json!({"type": "disabled"}),
            None,
        ),
    ] {
        let (body, has_beta) = send(&request(model, Some(effort))).await.unwrap();
        assert_eq!(body["thinking"], thinking, "{model} {effort}");
        assert_eq!(
            body.get("output_config").cloned(),
            output_config,
            "{model} {effort}"
        );
        assert!(body.get("context_management").is_none(), "{model}");
        assert!(
            !has_beta,
            "{model} must not require an anthropic-beta header"
        );
    }
}

// Given: the same supported selection / When: the request is streamed
// Then: the streaming wire carries the same controls as the non-streaming one.
#[tokio::test]
async fn supported_effort_is_sent_on_streaming_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse_response(&fixture("anthropic", "stream_text.sse")))
        .mount(&server)
        .await;
    let events = client(&server)
        .await
        .stream(
            &ProviderAuth::new("sk-ant-test"),
            &request("claude-sonnet-4-6", Some("medium")),
        )
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let captured = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&captured[0].body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(body["output_config"], json!({"effort": "medium"}));
}

// Given: no effort / When: the request is built
// Then: no reasoning field and the caller's temperature are untouched (default behavior).
#[tokio::test]
async fn unspecified_effort_keeps_the_default_wire() {
    for model in ["claude-sonnet-4-6", "claude-opus-4-7", "claude-sonnet-4-5"] {
        let mut input = request(model, None);
        input.temperature = Some(0.2);
        let (body, has_beta) = send(&input).await.unwrap();
        for field in ["thinking", "output_config", "context_management"] {
            assert!(body.get(field).is_none(), "{model} must not send {field}");
        }
        assert_eq!(body["temperature"], 0.2);
        assert!(!has_beta);
    }
}

// Given: a model or effort the capability table does not support
// When: an explicit effort is selected / Then: nothing reasoning-related is sent and
// the request still succeeds.
#[tokio::test]
async fn unsupported_model_or_effort_is_not_sent() {
    for (model, effort, max_tokens) in [
        ("some-gateway/claude", "high", 8192),
        ("claude-sonnet-4-6", "xhigh", 8192),
        ("claude-sonnet-4-5", "max", 8192),
        ("claude-mythos-preview", "none", 8192),
        ("claude-sonnet-4-5", "low", 1024),
    ] {
        let mut input = request(model, Some(effort));
        input.max_tokens = Some(max_tokens);
        input.temperature = Some(0.2);
        let (body, _) = send(&input).await.unwrap();
        for field in ["thinking", "output_config", "context_management"] {
            assert!(body.get(field).is_none(), "{model} {effort}: {field}");
        }
        assert_eq!(body["temperature"], 0.2, "{model} {effort}");
    }
}

// Given: thinking is enabled / When: a custom temperature is requested
// Then: only the default temperature is allowed, mirroring the Anthropic API rule.
#[tokio::test]
async fn enabled_thinking_requires_the_default_temperature() {
    let mut input = request("claude-sonnet-4-6", Some("high"));
    input.temperature = Some(0.2);
    assert!(matches!(send(&input).await, Err(ProviderError::Request(_))));
    input.temperature = Some(1.0);
    let (body, _) = send(&input).await.unwrap();
    assert!(body.get("temperature").is_none());
}
