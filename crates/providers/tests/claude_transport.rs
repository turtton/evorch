use futures_util::StreamExt;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::provider::claude::CLAUDE_USER_AGENT;
use providers::{ChatRequest, ProviderAuth, ProviderClient};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> ChatRequest {
    serde_json::from_value(json!({"model":"claude-sonnet-4-6", "messages":[
        {"role":"user","content":[{"type":"text","text":"Reply LOCAL_CAPTURE_OK."}]}
    ]}))
    .unwrap()
}

#[tokio::test]
async fn claude_decodes_every_advertised_compression_for_send_and_sse() {
    // Independent compressed fixtures generated with Node v24.21.0 zlib.
    let formats: [(&str, &[u8], &[u8]); 4] = [
        (
            "gzip",
            include_bytes!("../fixtures/claude-compression/send.gzip"),
            include_bytes!("../fixtures/claude-compression/stream.gzip"),
        ),
        (
            "deflate",
            include_bytes!("../fixtures/claude-compression/send.deflate"),
            include_bytes!("../fixtures/claude-compression/stream.deflate"),
        ),
        (
            "br",
            include_bytes!("../fixtures/claude-compression/send.br"),
            include_bytes!("../fixtures/claude-compression/stream.br"),
        ),
        (
            "zstd",
            include_bytes!("../fixtures/claude-compression/send.zstd"),
            include_bytes!("../fixtures/claude-compression/stream.zstd"),
        ),
    ];
    for (encoding, send, stream) in formats {
        let server = MockServer::start().await;
        for (route, content_type, body) in [
            ("/send/messages", "application/json", send),
            ("/stream/messages", "text/event-stream", stream),
        ] {
            Mock::given(method("POST"))
                .and(path(route))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", content_type)
                        .insert_header("content-encoding", encoding)
                        .set_body_bytes(body),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        let auth = ProviderAuth::new("dummy-token");
        let input = request();
        let client = |route: &str| {
            AnthropicClient::new(AnthropicConfig {
                base_url: format!("{}{route}", server.uri()),
                ..Default::default()
            })
            .unwrap()
            .with_claude_oauth()
            .unwrap()
        };
        let sent = client("/send").send(&auth, &input).await.unwrap();
        assert!(
            sent.message.content.iter().any(|block| matches!(block,
            providers::ContentBlock::Text {text} if text == "ok")),
            "{encoding}"
        );
        let events = client("/stream")
            .stream(&auth, &input)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok), "{encoding}: {events:?}");
        assert!(
            matches!(
                events.last(),
                Some(Ok(providers::StreamEvent::Completed { .. }))
            ),
            "{encoding}: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(event, Ok(providers::StreamEvent::TextDelta {text}) if text == "ok")), "{encoding}");
        for captured in server.received_requests().await.unwrap() {
            assert_eq!(
                captured.headers["accept-encoding"],
                "gzip, deflate, br, zstd"
            );
            mock_openai::claude_contract::claude_model_input(&captured.body).unwrap();
        }
    }
}

#[tokio::test]
async fn minimum_version_rejection_is_sanitized_without_mutating_the_profile() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":{
            "type":"invalid_request_error", "code":"claude_code_version_too_old",
            "message":"Claude Code version 2.1.300 or newer is required; secret-token account-secret"
        }}))).expect(2).mount(&server).await;
    let client = AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap()
    .with_claude_oauth()
    .unwrap();
    for _ in 0..2 {
        let error = client
            .send(&ProviderAuth::new("dummy-token"), &request())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, providers::ProviderError::Http {status:400,body} if body.contains("2.1.300"))
        );
        assert!(!error.to_string().contains("secret"));
    }
    let captured = server.received_requests().await.unwrap();
    for request in &captured {
        assert_eq!(request.headers["user-agent"], CLAUDE_USER_AGENT);
    }
    assert_eq!(captured[0].body, captured[1].body);
    mock_openai::claude_contract::claude_model_input(&captured[0].body).unwrap();
}

async fn claude_error(response: ResponseTemplate, token: &str) -> providers::ProviderError {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(response)
        .mount(&server)
        .await;
    let client = AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap()
    .with_claude_oauth()
    .unwrap();
    client
        .send(&ProviderAuth::new(token), &request())
        .await
        .unwrap_err()
}

// Given: tool_use.input 非 object を理由にした Claude の 400 / When: send / Then: type と message が表出し、使用中 token は除去される
#[tokio::test]
async fn claude_rejection_shows_type_and_message_without_the_token() {
    let token = "oauth-in-use-0123";
    let error = claude_error(
        ResponseTemplate::new(400).set_body_json(json!({"type":"error","error":{
            "type":"invalid_request_error",
            "message":format!("messages.1.content.0.tool_use.input: Input should be an object\n{token}")
        },"request_id":"req_leak"})),
        token,
    )
    .await;

    let text = error.to_string();
    assert!(
        matches!(&error, providers::ProviderError::Http { status: 400, body }
            if body == "Claude request rejected: invalid_request_error: messages.1.content.0.tool_use.input: Input should be an object ***"),
        "{text}"
    );
    assert!(
        !text.contains(token) && !text.contains("req_leak"),
        "{text}"
    );
    assert!(!error.is_structured_output_unsupported());
}

// Given: HTML・非 JSON・不正型・過大の 400 本文 / When: send / Then: 固定文言になり raw 本文を表示しない
#[tokio::test]
async fn claude_rejection_falls_back_without_echoing_raw_bodies() {
    let oversized = format!(
        r#"{{"error":{{"message":"raw-oversized-leak{}"}}}}"#,
        " ".repeat(1024 * 1024)
    );
    let bodies = [
        "<html><body>raw-html-leak</body></html>".to_owned(),
        "raw-plain-leak".to_owned(),
        r#"{"error":"raw-string-leak"}"#.to_owned(),
        r#"{"error":{"type":7,"message":["raw-array-leak"]}}"#.to_owned(),
        oversized,
    ];
    for body in bodies {
        let error = claude_error(
            ResponseTemplate::new(400).set_body_string(body),
            "tok-fallback-1",
        )
        .await;

        assert!(
            matches!(&error, providers::ProviderError::Http { status: 400, body } if body == "Claude request rejected"),
            "{error}"
        );
        assert!(!error.to_string().contains("leak"));
    }
}

// Given: Retry-After 付き 429 / When: send / Then: RateLimited と待機時間を保つ
#[tokio::test]
async fn claude_rate_limit_keeps_retry_after() {
    let error = claude_error(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "7")
            .set_body_json(json!({"error":{"type":"rate_limit_error","message":"slow down"}})),
        "tok-rate-limit-1",
    )
    .await;

    assert_eq!(
        error,
        providers::ProviderError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(7))
        }
    );
}

// Given: API キー経路の 400 / When: send / Then: 本文は生のまま分類に使われ、無関係な 400 は未対応と誤分類されない
#[tokio::test]
async fn api_key_path_keeps_raw_body_for_error_classification() {
    let cases = [
        (
            json!({"type":"error","error":{"type":"invalid_request_error",
                "message":"response_format json_schema is not supported by this model"}}),
            true,
        ),
        (
            json!({"type":"error","error":{"type":"invalid_request_error",
                "message":"messages.1.content.0.tool_use.input: Input should be an object"}}),
            false,
        ),
    ];
    for (body, unsupported) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let client = AnthropicClient::new(AnthropicConfig {
            base_url: server.uri(),
            ..Default::default()
        })
        .unwrap();

        let error = client
            .send(&ProviderAuth::new("sk-api-key-0123"), &request())
            .await
            .unwrap_err();

        assert!(
            matches!(&error, providers::ProviderError::Http { status: 400, body: raw }
                if serde_json::from_str::<serde_json::Value>(raw).ok() == Some(body.clone())),
            "{error}"
        );
        assert_eq!(error.is_structured_output_unsupported(), unsupported);
    }
}
