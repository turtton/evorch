// allow: SIZE_OK — OpenAI のHTTP/SSE契約を1つの統合テストバイナリで網羅する契約表です。
mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{EventBus, ProviderEvent, UsageEvent};
use futures_util::StreamExt;
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderCapabilities,
    ProviderClient, ProviderError, Role, StreamEvent, Usage,
};
use serde_json::json;
use support::{fixture, json_response, next_provider_event, next_usage_event, sse_response};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gpt-contract";

fn request() -> ChatRequest {
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
        max_tokens: None,
    }
}

fn client(
    server: &MockServer,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
) -> OpenAiClient {
    OpenAiClient::new(OpenAiConfig {
        base_url: server.uri(),
        timeout,
        event_bus,
    })
    .expect("OpenAI client を構築できる")
}

async fn mount(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer sk-contract"))
        .respond_with(response)
        .mount(server)
        .await;
}

fn expected_usage() -> Usage {
    Usage {
        input_tokens: 11,
        output_tokens: 7,
        cache_read_tokens: 3,
        cache_write_tokens: 0,
    }
}

fn assert_usage_event(event: UsageEvent, provider: &str, usage: Usage) {
    match event {
        UsageEvent::Usage {
            provider: actual_provider,
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
        } => {
            assert_eq!(actual_provider, provider);
            assert_eq!(model, MODEL);
            assert_eq!(input_tokens, usage.input_tokens);
            assert_eq!(output_tokens, usage.output_tokens);
            assert_eq!(cache_read_tokens, usage.cache_read_tokens);
            assert_eq!(cache_write_tokens, usage.cache_write_tokens);
        }
        UsageEvent::CacheStats { .. } => panic!("Usage イベントを期待しました"),
    }
}

// Given: OpenAI text応答 / When: send / Then: canonical応答、usage、capabilitiesへ変換される
#[tokio::test(flavor = "multi_thread")]
async fn send_text_maps_response_and_capabilities() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(200, &fixture("openai", "send_text.json")),
    )
    .await;
    let client = client(&server, Duration::from_secs(1), None);

    let response = client
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("send は成功する");

    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "Hello from OpenAI.".to_string()
        }]
    );
    assert_eq!(response.usage, expected_usage());
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        client.capabilities(),
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: false
        }
    );
}

// Given: OpenAI tool call応答 / When: send / Then: JSON引数を持つcanonical ToolUseになる
#[tokio::test(flavor = "multi_thread")]
async fn send_tool_call_maps_canonical_tool_use() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(200, &fixture("openai", "send_tool_call.json")),
    )
    .await;

    let response = client(&server, Duration::from_secs(1), None)
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("send は成功する");

    assert_eq!(
        response.message.content,
        vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            input: json!({"location": "Tokyo"})
        }]
    );
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
}

// Given: text差分とusageとDONE / When: streamを収集 / Then: 差分列と累積Completedになる
#[tokio::test(flavor = "multi_thread")]
async fn stream_text_emits_deltas_and_completed_response() {
    let server = MockServer::start().await;
    mount(&server, sse_response(&fixture("openai", "stream_text.sse"))).await;

    let events = client(&server, Duration::from_secs(1), None)
        .stream(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("stream を開始できる")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("stream は成功する");

    assert_eq!(
        &events[..3],
        &[
            StreamEvent::TextDelta {
                text: "Hello".into()
            },
            StreamEvent::TextDelta {
                text: " from".into()
            },
            StreamEvent::TextDelta {
                text: " OpenAI.".into()
            }
        ]
    );
    assert_eq!(
        events[3],
        StreamEvent::Completed {
            response: providers::ChatResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Hello from OpenAI.".into()
                    }]
                },
                usage: expected_usage(),
                finish_reason: FinishReason::Stop
            }
        }
    );
}

// Given: 分割されたtool call差分 / When: streamを収集 / Then: 断片と再構築済みToolUseになる
#[tokio::test(flavor = "multi_thread")]
async fn stream_tool_call_emits_fragments_and_completed_tool_use() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "stream_tool_call.sse")),
    )
    .await;

    let events = client(&server, Duration::from_secs(1), None)
        .stream(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("stream を開始できる")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("stream は成功する");

    assert_eq!(
        &events[..3],
        &[
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("get_weather".into()),
                arguments_delta: "{\"location\":".into()
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "\"Tok".into()
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "yo\"}".into()
            }
        ]
    );
    let StreamEvent::Completed { response } = &events[3] else {
        panic!("最後のイベントは Completed である必要がある");
    };
    assert_eq!(
        response.message.content,
        vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "get_weather".into(),
            input: json!({"location": "Tokyo"})
        }]
    );
    assert_eq!(response.usage.input_tokens, 13);
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
}

// Given: send用EventBus購読 / When: send / Then: openaiラベルのusageが4フィールド届く
#[tokio::test(flavor = "multi_thread")]
async fn send_emits_usage_event() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(200, &fixture("openai", "send_text.json")),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();

    client(&server, Duration::from_secs(1), Some(bus))
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("send は成功する");

    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::RequestStarted {
            streaming: false,
            ..
        }
    ));
    assert_usage_event(
        next_usage_event(&mut receiver).await,
        "openai",
        expected_usage(),
    );
    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::RequestCompleted {
            streaming: false,
            ..
        }
    ));
}

// Given: stream用EventBus購読 / When: DONEまで収集 / Then: openaiラベルのusageが4フィールド届く
#[tokio::test(flavor = "multi_thread")]
async fn stream_emits_usage_event() {
    let server = MockServer::start().await;
    mount(&server, sse_response(&fixture("openai", "stream_text.sse"))).await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();

    let _: Vec<_> = client(&server, Duration::from_secs(1), Some(bus))
        .stream(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("stream を開始できる")
        .collect()
        .await;

    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::RequestStarted {
            streaming: true,
            ..
        }
    ));
    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::FirstTokenObserved { .. }
    ));
    assert_usage_event(
        next_usage_event(&mut receiver).await,
        "openai",
        expected_usage(),
    );
    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::RequestCompleted {
            streaming: true,
            ..
        }
    ));
}

async fn assert_http_error(status: u16) {
    let server = MockServer::start().await;
    mount(&server, json_response(status, r#"{"error":"failed"}"#)).await;
    let error = client(&server, Duration::from_secs(1), None)
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect_err("HTTPエラーになる");
    assert!(matches!(error, ProviderError::Http { status: actual, .. } if actual == status));
}

// Given: 400応答 / When: send / Then: status 400のHttpエラーになる
#[tokio::test(flavor = "multi_thread")]
async fn status_400_maps_to_http_error() {
    assert_http_error(400).await;
}

// Given: Retry-After付き429 / When: send / Then: 2秒付きRateLimitedになる
#[tokio::test(flavor = "multi_thread")]
async fn status_429_with_retry_after_maps_to_rate_limited() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(429, r#"{"error":"limited"}"#).insert_header("Retry-After", "2"),
    )
    .await;

    let error = client(&server, Duration::from_secs(1), None)
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect_err("429になる");

    assert_eq!(
        error,
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(2))
        }
    );
}

// Given: Retry-Afterなし429 / When: send / Then: 待機時間なしRateLimitedになる
#[tokio::test(flavor = "multi_thread")]
async fn status_429_without_retry_after_maps_to_rate_limited() {
    let server = MockServer::start().await;
    mount(&server, json_response(429, r#"{"error":"limited"}"#)).await;

    let error = client(&server, Duration::from_secs(1), None)
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect_err("429になる");

    assert_eq!(error, ProviderError::RateLimited { retry_after: None });
}

// Given: 500応答 / When: send / Then: status 500のHttpエラーになる
#[tokio::test(flavor = "multi_thread")]
async fn status_500_maps_to_http_error() {
    assert_http_error(500).await;
}

// Given: 不正JSONのSSE frame / When: streamを収集 / Then: InvalidJsonがitemとして返る
#[tokio::test(flavor = "multi_thread")]
async fn malformed_sse_json_yields_invalid_json_error() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response("data: {invalid json}\n\ndata: [DONE]\n\n"),
    )
    .await;

    let events = client(&server, Duration::from_secs(1), None)
        .stream(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("HTTP stream は開始できる")
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.as_slice(),
        [Err(ProviderError::InvalidJson { .. })]
    ));
}

// Given: 100ms timeoutと2秒遅延 / When: send / Then: Timeoutになる
#[tokio::test(flavor = "multi_thread")]
async fn delayed_send_response_times_out() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(200, &fixture("openai", "send_text.json")).set_delay(Duration::from_secs(2)),
    )
    .await;

    let error = client(&server, Duration::from_millis(100), None)
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect_err("send はタイムアウトする");

    assert_eq!(error, ProviderError::Timeout);
}
