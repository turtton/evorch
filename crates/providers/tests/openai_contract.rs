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

#[tokio::test]
async fn cache_key_is_serialized_when_run_is_set_and_omitted_otherwise() {
    // Given: requests with and without an observation run.
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_text.sse")),
    )
    .await;
    let client = client(&server, Duration::from_secs(1), None);
    for run in [Some("run-cache-affinity"), None] {
        let mut input = request();
        input.observation = run.map(|id| providers::ObservationContext {
            turn: None,
            run_id: id.into(),
            purpose: Default::default(),
        });
        // When: sent through the real HTTP adapter.
        client
            .send(&ProviderAuth::new("sk-contract"), &input)
            .await
            .unwrap();
    }
    // Then: only the configured request contains the exact run ID.
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies[0]["prompt_cache_key"], "run-cache-affinity");
    assert!(bodies[1].get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn cache_tools_are_byte_stable_when_registration_order_changes() {
    // Given: the same named tool set in opposite registration orders.
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_text.sse")),
    )
    .await;
    let mut input = request();
    input.tools = ["zeta", "alpha"]
        .map(|name| providers::ToolSpec {
            name: name.into(),
            description: name.into(),
            input_schema: json!({"type":"object"}),
        })
        .to_vec();
    let client = client(&server, Duration::from_secs(1), None);
    // When: both permutations pass through the HTTP surface.
    client
        .send(&ProviderAuth::new("sk-contract"), &input)
        .await
        .unwrap();
    input.tools.reverse();
    client
        .send(&ProviderAuth::new("sk-contract"), &input)
        .await
        .unwrap();
    // Then: the serialized tool arrays are byte-identical.
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        serde_json::to_vec(&bodies[0]["tools"]).unwrap(),
        serde_json::to_vec(&bodies[1]["tools"]).unwrap()
    );
}

fn request() -> ChatRequest {
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
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
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
        .and(path("/responses"))
        .and(header("authorization", "Bearer sk-contract"))
        .respond_with(response)
        .mount(server)
        .await;
}

fn expected_usage() -> Usage {
    Usage {
        context_tokens: None,
        input_tokens: 11,
        output_tokens: 7,
        cache_read_tokens: 3,
        cache_write_tokens: 0,
        reasoning_tokens: Some(2),
    }
}

fn expected_text_content() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Reasoning {
            text: "Greeting the user.".to_string(),
        },
        ContentBlock::Text {
            text: "Hello from OpenAI.".to_string(),
        },
    ]
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

// Given: Responses SSE / When: send / Then: canonical応答、usage、capabilitiesへ変換される
#[tokio::test(flavor = "multi_thread")]
async fn send_text_maps_response_and_capabilities() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_text.sse")),
    )
    .await;
    let client = client(&server, Duration::from_secs(1), None);

    let response = client
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("send は成功する");

    assert_eq!(response.message.content, expected_text_content());
    assert_eq!(response.usage, expected_usage());
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        client.capabilities(),
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: true
        }
    );
}

// Given: OpenAI tool call応答 / When: send / Then: JSON引数を持つcanonical ToolUseになる
#[tokio::test(flavor = "multi_thread")]
async fn send_tool_call_maps_canonical_tool_use() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_tool_call.sse")),
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

// Given: reasoning summaryとtext差分とresponse.completed / When: streamを収集 / Then: 差分列と累積Completedになる
#[tokio::test(flavor = "multi_thread")]
async fn stream_text_emits_deltas_and_completed_response() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_text.sse")),
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
        &events[..4],
        &[
            StreamEvent::ReasoningDelta {
                text: "Greeting the user.".into()
            },
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
        events[4..],
        [StreamEvent::Completed {
            response: providers::ChatResponse {
                message: Message {
                    role: Role::Assistant,
                    content: expected_text_content(),
                },
                usage: expected_usage(),
                finish_reason: FinishReason::Stop
            }
        }]
    );
}

// Given: 分割されたtool call差分 / When: streamを収集 / Then: 断片と再構築済みToolUseになる
#[tokio::test(flavor = "multi_thread")]
async fn stream_tool_call_emits_fragments_and_completed_tool_use() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_tool_call.sse")),
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

    let (completed, deltas) = events.split_last().expect("イベントがある");
    assert_eq!(
        deltas.first(),
        Some(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("get_weather".into()),
            arguments_delta: String::new(),
        }),
        "output_item.added が呼び出しの識別子を先に運ぶ"
    );
    let fragments: Vec<&str> = deltas
        .iter()
        .map(|event| match event {
            StreamEvent::ToolCallDelta {
                index: 0,
                arguments_delta,
                ..
            } => arguments_delta.as_str(),
            other => panic!("tool call 差分以外は来ない: {other:?}"),
        })
        .filter(|fragment| !fragment.is_empty())
        .collect();
    assert_eq!(fragments, ["{\"location\":", "\"Tok", "yo\"}"]);
    let StreamEvent::Completed { response } = completed else {
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
    assert_eq!(response.usage.cache_read_tokens, 4);
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
}

// Given: send用EventBus購読 / When: send / Then: openaiラベルのusageが4フィールド届く
#[tokio::test(flavor = "multi_thread")]
async fn send_emits_usage_event() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_text.sse")),
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
    mount(
        &server,
        sse_response(&fixture("openai", "responses_text.sse")),
    )
    .await;
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
        sse_response("event: response.output_text.delta\ndata: {invalid json}\n\n"),
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
        sse_response(&fixture("openai", "responses_text.sse")).set_delay(Duration::from_secs(2)),
    )
    .await;

    let error = client(&server, Duration::from_millis(100), None)
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect_err("send はタイムアウトする");

    assert_eq!(error, ProviderError::Timeout);
}

// Given: system・tool・推論強度・上限を持つ要求 / When: send と stream で送信
// Then: 公式 Responses endpoint へ SSE を要求し、Codex 固有ヘッダを付けず、同じ本文を送る
#[tokio::test(flavor = "multi_thread")]
async fn requests_use_the_official_responses_endpoint_and_shape() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_text.sse")),
    )
    .await;
    let mut input = request();
    input.messages.insert(
        0,
        Message {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: "Be accurate.".into(),
            }],
        },
    );
    input.tools = vec![providers::ToolSpec {
        name: "get_weather".into(),
        description: "Weather".into(),
        input_schema: json!({"type": "object"}),
    }];
    input.reasoning_effort = Some("low".into());
    input.max_tokens = Some(512);
    input.temperature = Some(0.5);
    input.observation = Some(providers::ObservationContext {
        turn: None,
        run_id: "run-shape".into(),
        purpose: Default::default(),
    });
    let client = client(&server, Duration::from_secs(1), None);
    let auth = ProviderAuth::new("sk-contract");

    client.send(&auth, &input).await.expect("send は成功する");
    let _: Vec<_> = client
        .stream(&auth, &input)
        .await
        .expect("stream を開始できる")
        .collect()
        .await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.headers["accept"], "text/event-stream");
        for codex_only in [
            "chatgpt-account-id",
            "originator",
            "openai-beta",
            "session-id",
        ] {
            assert!(!request.headers.contains_key(codex_only), "{codex_only}");
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body,
            json!({
                "model": MODEL,
                "instructions": "Be accurate.",
                "input": [{"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}]}],
                "tools": [{"type": "function", "name": "get_weather", "description": "Weather",
                    "parameters": {"type": "object"}, "strict": false}],
                "store": false,
                "stream": true,
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "reasoning": {"effort": "low", "summary": "auto"},
                "max_output_tokens": 512,
                "temperature": 0.5,
                "prompt_cache_key": "run-shape",
                "include": ["reasoning.encrypted_content"]
            })
        );
    }
}

// Given: max_output_tokens で打ち切られた SSE / When: send / Then: 部分応答を Length で返す
#[tokio::test(flavor = "multi_thread")]
async fn incomplete_response_completes_with_length() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_incomplete.sse")),
    )
    .await;

    let response = client(&server, Duration::from_secs(1), None)
        .send(&ProviderAuth::new("sk-contract"), &request())
        .await
        .expect("打ち切りは完了として扱う");

    assert_eq!(response.finish_reason, FinishReason::Length);
    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "Truncated answer".into()
        }]
    );
    assert_eq!(response.usage.output_tokens, 16);
}

// Given: stream 途中のトップレベル error event / When: stream / Then: code 付き Http 400 が item になる
#[tokio::test(flavor = "multi_thread")]
async fn error_event_yields_http_error_item() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("openai", "responses_error_event.sse")),
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
        [Err(ProviderError::Http { status: 400, body })]
            if body.starts_with("context_length_exceeded: ")
    ));
}
