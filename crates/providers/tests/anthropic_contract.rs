// allow: SIZE_OK — Anthropic の HTTP/SSE 契約を 1 統合テストバイナリで網羅する。
mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{EventBus, ProviderEvent, UsageEvent};
use futures_util::StreamExt;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderCapabilities,
    ProviderClient, ProviderError, Role, StreamEvent, Usage,
};
use serde_json::json;
use support::{fixture, json_response, next_provider_event, next_usage_event, sse_response};
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "claude-test";
const API_KEY: &str = "sk-ant-test";

#[derive(Debug)]
struct AnthropicBodyMatcher {
    stream: bool,
}

impl Match for AnthropicBodyMatcher {
    fn matches(&self, request: &Request) -> bool {
        let body: serde_json::Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(_) => return false,
        };
        body["model"] == MODEL
            && body["max_tokens"].as_u64().is_some_and(|value| value > 0)
            && body["stream"] == self.stream
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "こんにちは".to_string(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
    }
}

fn client(server: &MockServer, timeout: Duration) -> AnthropicClient {
    AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        timeout,
        event_bus: None,
    })
    .expect("Anthropic client を構築できる")
}

fn client_with_bus(server: &MockServer, event_bus: Arc<EventBus>) -> AnthropicClient {
    AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        timeout: Duration::from_secs(1),
        event_bus: Some(event_bus),
    })
    .expect("Anthropic client を構築できる")
}

async fn mount(server: &MockServer, response: ResponseTemplate, stream: bool) {
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("x-api-key", API_KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("content-type", "application/json"))
        .and(AnthropicBodyMatcher { stream })
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

fn assert_usage_event(event: UsageEvent, expected: Usage) {
    match event {
        UsageEvent::Usage {
            provider,
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
        } => {
            assert_eq!(provider, "anthropic");
            assert_eq!(model, MODEL);
            assert_eq!(
                Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                },
                expected
            );
        }
        UsageEvent::CacheStats { .. } => panic!("Usage token イベントを期待した"),
    }
}

// Given: Anthropic のテキスト応答とキャッシュ usage / When: send / Then: canonical 応答・機能・要求形状が保存される
#[tokio::test]
async fn send_text_maps_response_usage_and_capabilities() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(200, &fixture("anthropic", "send_text.json")),
        false,
    )
    .await;
    let client = client(&server, Duration::from_secs(1));

    let response = client
        .send(&ProviderAuth::new(API_KEY), &request())
        .await
        .expect("send は成功する");

    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "こんにちは。".to_string()
        }]
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 9,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_write_tokens: 4,
        }
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        client.capabilities(),
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: true,
        }
    );
}

// Given: Anthropic の tool_use 応答 / When: send / Then: canonical ToolUse と JSON input へ変換される
#[tokio::test]
async fn send_tool_use_maps_canonical_block() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(200, &fixture("anthropic", "send_tool_use.json")),
        false,
    )
    .await;

    let response = client(&server, Duration::from_secs(1))
        .send(&ProviderAuth::new(API_KEY), &request())
        .await
        .expect("send は成功する");

    assert_eq!(
        response.message.content,
        vec![ContentBlock::ToolUse {
            id: "toolu_1".to_string(),
            name: "get_weather".to_string(),
            input: json!({"city": "Tokyo", "unit": "celsius"}),
        }]
    );
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
}

// Given: テキスト SSE / When: stream を全件収集 / Then: 差分列と累積 Completed が返る
#[tokio::test]
async fn stream_text_emits_deltas_and_completed_response() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("anthropic", "stream_text.sse")),
        true,
    )
    .await;

    let events = client(&server, Duration::from_millis(100))
        .stream(&ProviderAuth::new(API_KEY), &request())
        .await
        .expect("stream を開始できる")
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0],
        Ok(StreamEvent::TextDelta {
            text: "こん".to_string()
        })
    );
    assert_eq!(
        events[1],
        Ok(StreamEvent::TextDelta {
            text: "にちは。".to_string()
        })
    );
    let StreamEvent::Completed { response } = events[2].as_ref().expect("完了イベントは Ok")
    else {
        panic!("最後のイベントは Completed のはず")
    };
    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "こんにちは。".to_string()
        }]
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 9,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_write_tokens: 4,
        }
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

// Given: tool_use SSE の分割 JSON / When: stream を全件収集 / Then: ToolCallDelta と統合済み ToolUse が返る
#[tokio::test]
async fn stream_tool_use_reassembles_json_arguments() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("anthropic", "stream_tool_use.sse")),
        true,
    )
    .await;

    let events = client(&server, Duration::from_secs(1))
        .stream(&ProviderAuth::new(API_KEY), &request())
        .await
        .expect("stream を開始できる")
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events.len(), 4);
    assert_eq!(
        events[0],
        Ok(StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("toolu_1".to_string()),
            name: Some("get_weather".to_string()),
            arguments_delta: String::new(),
        })
    );
    assert!(matches!(events[1], Ok(StreamEvent::ToolCallDelta { .. })));
    assert!(matches!(events[2], Ok(StreamEvent::ToolCallDelta { .. })));
    let StreamEvent::Completed { response } = events[3].as_ref().expect("完了イベントは Ok")
    else {
        panic!("最後のイベントは Completed のはず")
    };
    assert_eq!(
        response.message.content,
        vec![ContentBlock::ToolUse {
            id: "toolu_1".to_string(),
            name: "get_weather".to_string(),
            input: json!({"city": "Tokyo", "unit": "celsius"}),
        }]
    );
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
}

// Given: EventBus 接続済み client / When: send / Then: 4 token field を持つ usage が 1 件届く
#[tokio::test]
async fn send_emits_usage_event() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(200, &fixture("anthropic", "send_text.json")),
        false,
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();

    client_with_bus(&server, bus)
        .send(&ProviderAuth::new(API_KEY), &request())
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
        Usage {
            input_tokens: 9,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_write_tokens: 4,
        },
    );
    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::RequestCompleted {
            streaming: false,
            ..
        }
    ));
}

// Given: EventBus 接続済み client / When: stream を完了まで読む / Then: 4 token field を持つ usage が 1 件届く
#[tokio::test]
async fn stream_emits_usage_event() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("anthropic", "stream_text.sse")),
        true,
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();

    let events = client_with_bus(&server, bus)
        .stream(&ProviderAuth::new(API_KEY), &request())
        .await
        .expect("stream を開始できる")
        .collect::<Vec<_>>()
        .await;

    assert!(events.iter().all(Result::is_ok));
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
        Usage {
            input_tokens: 9,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_write_tokens: 4,
        },
    );
    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::RequestCompleted {
            streaming: true,
            ..
        }
    ));
}

// Given: 400/429/500 応答 / When: send / Then: 共通 HTTP エラー写像が適用される
#[tokio::test]
async fn send_maps_http_error_statuses() {
    for (status, retry_after, expected) in [
        (
            400,
            None,
            ProviderError::Http {
                status: 400,
                body: "bad request".to_string(),
            },
        ),
        (
            429,
            Some("2"),
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(2)),
            },
        ),
        (
            500,
            None,
            ProviderError::Http {
                status: 500,
                body: "server error".to_string(),
            },
        ),
    ] {
        let server = MockServer::start().await;
        let mut response = ResponseTemplate::new(status).set_body_string(match status {
            400 => "bad request",
            429 => "slow down",
            500 => "server error",
            _ => unreachable!("テストケースは列挙済み"),
        });
        if let Some(value) = retry_after {
            response = response.insert_header("Retry-After", value);
        }
        mount(&server, response, false).await;

        let error = client(&server, Duration::from_secs(1))
            .send(&ProviderAuth::new(API_KEY), &request())
            .await
            .expect_err("HTTP エラーになる");

        assert_eq!(error, expected);
    }
}

// Given: 壊れた JSON を持つ SSE / When: stream を読む / Then: ストリーム item が Err になる
#[tokio::test]
async fn malformed_sse_yields_error() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response("event: content_block_delta\ndata: {broken\n\n"),
        true,
    )
    .await;

    let events = client(&server, Duration::from_secs(1))
        .stream(&ProviderAuth::new(API_KEY), &request())
        .await
        .expect("HTTP stream は開始できる")
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.as_slice(),
        [Err(ProviderError::InvalidJson { .. })]
    ));
}

// Given: Anthropic error イベント / When: stream を読む / Then: in-stream HTTP エラーになる
#[tokio::test]
async fn in_stream_error_event_yields_error() {
    let server = MockServer::start().await;
    let body = concat!(
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}\n\n"
    );
    mount(&server, sse_response(body), true).await;

    let events = client(&server, Duration::from_secs(1))
        .stream(&ProviderAuth::new(API_KEY), &request())
        .await
        .expect("HTTP stream は開始できる")
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.as_slice(),
        [Err(ProviderError::Http { status: 400, .. })]
    ));
}

// Given: 2 秒遅延する応答と 100ms timeout / When: send / Then: Timeout になる
#[tokio::test]
async fn send_timeout_maps_to_timeout_error() {
    let server = MockServer::start().await;
    mount(
        &server,
        json_response(200, &fixture("anthropic", "send_text.json"))
            .set_delay(Duration::from_secs(2)),
        false,
    )
    .await;

    let error = client(&server, Duration::from_millis(100))
        .send(&ProviderAuth::new(API_KEY), &request())
        .await
        .expect_err("send は timeout する");

    assert_eq!(error, ProviderError::Timeout);
}
