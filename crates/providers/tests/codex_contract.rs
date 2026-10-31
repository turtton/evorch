#[path = "support/codex.rs"]
mod codex_support;
#[path = "support/codex_contract.rs"]
mod contract_support;
#[path = "support/codex_overload_contract.rs"]
mod overload_contract;
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
            format!("codex_cli_rs/{}", providers::CODEX_CLIENT_VERSION),
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

// Given: a request with a system prompt / When: it is sent to the Codex backend
// Then: the HTTP body has no top-level `instructions` and input[0] is the developer message
#[tokio::test]
async fn system_prompt_reaches_the_backend_as_a_developer_input_message() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("codex", "responses_success.sse")),
    )
    .await;
    let client = client(&server, seeded_store());
    let mut with_system = request();
    with_system.messages.insert(
        0,
        providers::Message {
            role: providers::Role::System,
            content: vec![ContentBlock::Text {
                text: "Follow the rules.".to_string(),
            }],
        },
    );

    client
        .send(&ProviderAuth::new(""), &with_system)
        .await
        .expect("send succeeds");

    let requests = server
        .received_requests()
        .await
        .expect("requests are recorded");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("instructions").is_none());
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(body["input"][0]["content"][0]["text"], "Follow the rules.");
    assert_eq!(body["input"][1]["role"], "user");
}

#[tokio::test]
async fn built_in_version_reaches_catalog_and_inference_headers_without_lookup() {
    // Given: only the Codex backend is mocked; any other endpoint would be unmatched.
    let server = MockServer::start().await;
    let version = providers::CODEX_CLIENT_VERSION;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(wiremock::matchers::query_param("client_version", version))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models": [{"slug": "gpt-4o"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("version", version))
        .and(header("user-agent", format!("codex_cli_rs/{version}")))
        .respond_with(sse_response(&fixture("codex", "responses_success.sse")))
        .expect(2)
        .mount(&server)
        .await;
    let client = CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            ..CodexConfig::default()
        },
        seeded_store(),
    )
    .expect("Codex client can be built");

    // When: the catalog is fetched and two inference turns are sent.
    assert_eq!(
        client.list_models(&ProviderAuth::new("")).await.unwrap(),
        Some(vec!["gpt-4o".into()])
    );
    for _ in 0..2 {
        let response = client
            .send(&ProviderAuth::new(""), &request())
            .await
            .expect("send succeeds with the built-in version");
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    // Then: every request carried the same built-in version, and nothing else
    // (such as a release lookup) was contacted.
    server.verify().await;
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
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
            context_tokens: None,
            input_tokens: 12,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
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

// Given: interleaved read calls without complete argument deltas / When: streamed over HTTP
// Then: the final provider response recovers each path without malformed-argument diagnostics
#[tokio::test]
async fn stream_parallel_tool_calls_recover_final_arguments() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("codex", "responses_parallel_tool_calls.sse")),
    )
    .await;
    let bus = Arc::new(EventBus::new(64));
    let mut diagnostics = bus.subscribe();
    let client = CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            event_bus: Some(Arc::clone(&bus)),
            ..CodexConfig::default()
        },
        seeded_store(),
    )
    .expect("Codex client can be built");
    let events = client
        .stream(&ProviderAuth::new(""), &request())
        .await
        .expect("stream starts")
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let StreamEvent::Completed { response } = events.last().unwrap().as_ref().unwrap() else {
        panic!("final event must be Completed")
    };
    assert_eq!(
        response.message.content,
        vec![
            ContentBlock::ToolUse {
                id: "call-read-1".into(),
                name: "read".into(),
                input: json!({"path":"Cargo.toml","limit":10}),
            },
            ContentBlock::ToolUse {
                id: "call-read-2".into(),
                name: "read".into(),
                input: json!({"path":"crates/providers/Cargo.toml","offset":2}),
            },
        ]
    );
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    bus.emit(event_bus::Event::new(
        event_bus::MessageEvent::MessageDelta {
            delta: "stream complete".into(),
            run_id: None,
        },
    ));
    loop {
        let event = diagnostics
            .recv()
            .await
            .expect("event receiver must not lag");
        if matches!(&event.kind, event_bus::EventKind::Message(event_bus::MessageEvent::MessageDelta { delta, .. }) if delta == "stream complete")
        {
            break;
        }
        assert!(!matches!(event.kind, event_bus::EventKind::Diagnostic(_)));
    }
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
