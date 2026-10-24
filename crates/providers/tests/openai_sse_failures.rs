//! 公式 OpenAI Responses の SSE 内失敗が、再試行・レート制限・観測の既存分類へ
//! 乗ることを実 HTTP 経由で固定する契約テストです。
pub mod support;

use std::sync::Arc;

use event_bus::{EventBus, EventKind, ProviderEvent, ProviderFailureKind};
use futures_util::FutureExt;
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{ChatRequest, ProviderAuth, ProviderClient, ProviderError};
use serde_json::json;
use support::sse_response;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-contract".into(),
        messages: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}

/// 部分出力の後に失敗イベントで終わる SSE 本文。
fn failing_sse(event: &str, code: &str, message: &str) -> String {
    let delta = json!({"type": "response.output_text.delta", "delta": "partial"});
    let failure = match event {
        "error" => json!({"type": "error", "code": code, "message": message}),
        _ => json!({"type": "response.failed",
            "response": {"status": "failed", "error": {"code": code, "message": message}}}),
    };
    format!(
        "event: response.output_text.delta\ndata: {delta}\n\nevent: {event}\ndata: {failure}\n\n"
    )
}

/// 失敗 SSE を返し続ける server へ send_streaming し、結果・要求数・観測を返す。
async fn run(
    body: String,
) -> (
    Result<providers::ChatResponse, ProviderError>,
    usize,
    Vec<EventKind>,
) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(sse_response(&body))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(256));
    let mut receiver = bus.subscribe();
    let client = OpenAiClient::new(OpenAiConfig {
        base_url: server.uri(),
        event_bus: Some(bus.clone()),
        ..OpenAiConfig::default()
    })
    .expect("OpenAI client");

    let result = client
        .send_streaming(&ProviderAuth::new("sk-contract"), &request(), &bus)
        .await;

    let requests = server.received_requests().await.unwrap().len();
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().now_or_never() {
        events.push(event.expect("観測イベント").kind);
    }
    (result, requests, events)
}

fn failures(events: &[EventKind]) -> Vec<ProviderFailureKind> {
    events
        .iter()
        .filter_map(|event| match event {
            EventKind::Provider(ProviderEvent::RequestFailed { failure, .. }) => Some(*failure),
            _ => None,
        })
        .collect()
}

fn assert_no_completion(events: &[EventKind]) {
    assert!(
        !events.iter().any(|event| matches!(
            event,
            EventKind::Usage(_) | EventKind::Provider(ProviderEvent::RequestCompleted { .. })
        )),
        "{events:?}"
    );
}

// Given: SSE responses that always fail mid-stream with server_error
// When: send_streaming runs with the default retry policy
// Then: every attempt is retried as a 5xx until exhaustion, each attempt is observed as an
// HTTP 500 failure, and no usage or completion is emitted
#[tokio::test]
async fn sse_server_error_is_retried_as_server_failure() {
    for event in ["error", "response.failed"] {
        let (result, requests, events) = run(failing_sse(
            event,
            "server_error",
            "The server had an error.",
        ))
        .await;

        let last = ProviderError::Http {
            status: 500,
            body: "server_error: The server had an error.".into(),
        };
        assert_eq!(
            result.unwrap_err(),
            ProviderError::RetriesExhausted {
                attempts: 3,
                last: Box::new(last),
            },
            "{event}"
        );
        assert_eq!(requests, 3, "{event}");
        assert_eq!(
            failures(&events),
            vec![ProviderFailureKind::Http { status: 500 }; 3],
            "{event}"
        );
        assert_no_completion(&events);
    }
}

// Given: SSE responses that fail mid-stream with rate_limit_exceeded
// When: send_streaming runs
// Then: the failure surfaces once as RateLimited (left to routing), is observed as a rate limit,
// and no usage or completion is emitted
#[tokio::test]
async fn sse_rate_limit_is_classified_as_rate_limited() {
    for event in ["error", "response.failed"] {
        let (result, requests, events) = run(failing_sse(
            event,
            "rate_limit_exceeded",
            "Rate limit reached.",
        ))
        .await;

        assert_eq!(
            result.unwrap_err(),
            ProviderError::RateLimited { retry_after: None },
            "{event}"
        );
        assert_eq!(requests, 1, "{event}");
        assert_eq!(
            failures(&events),
            vec![ProviderFailureKind::RateLimited],
            "{event}"
        );
        assert_no_completion(&events);
    }
}

// Given: SSE responses that fail mid-stream with an ordinary input error
// When: send_streaming runs
// Then: the request is not retried, it is observed as an HTTP 400 failure, and no usage or
// completion is emitted
#[tokio::test]
async fn sse_input_error_is_not_retried() {
    for event in ["error", "response.failed"] {
        let (result, requests, events) = run(failing_sse(
            event,
            "context_length_exceeded",
            "Your input exceeds the context window of this model.",
        ))
        .await;

        assert_eq!(
            result.unwrap_err(),
            ProviderError::Http {
                status: 400,
                body:
                    "context_length_exceeded: Your input exceeds the context window of this model."
                        .into(),
            },
            "{event}"
        );
        assert_eq!(requests, 1, "{event}");
        assert_eq!(
            failures(&events),
            vec![ProviderFailureKind::Http { status: 400 }],
            "{event}"
        );
        assert_no_completion(&events);
    }
}
