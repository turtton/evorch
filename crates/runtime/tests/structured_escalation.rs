//! Offline review contracts through runtime routing and the real provider HTTP adapters.

#[path = "structured_escalation/support.rs"]
mod support;

use std::time::Duration;

use event_bus::EventBus;
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::{ContentBlock, Message, Role as MessageRole, ToolSpec};
use runtime::escalation_review::{QuickModelReviewer, ReviewError, ReviewVerdict};
use runtime::{AgentInvocationContext, Role};
use serde_json::{Value, json};
use support::{harness, harness_fallback};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gpt-4o";
const APPROVE: &str = r#"{"approve":true,"reason":"safe inspection"}"#;
const DENY: &str = r#"{"approve":false,"reason":"unsafe effects"}"#;

async fn server(codex: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(if codex {
            json!({"models":[{"slug":MODEL}]})
        } else {
            json!({"data":[{"id":MODEL}]})
        }))
        .mount(&server)
        .await;
    if codex {
        Mock::given(method("GET"))
            .and(path("/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"tag_name":"rust-v0.156.1","draft":false,"prerelease":false}
            ])))
            .expect(1)
            .mount(&server)
            .await;
    }
    server
}

fn completion(text: &str) -> ResponseTemplate {
    let mut response = ScriptedResponse::text_stream("review", MODEL, [text])
        .with_usage(10, 3)
        .to_non_streaming_json();
    response["choices"][0]["message"]["reasoning_content"] = json!("Consider command effects.");
    ResponseTemplate::new(200).set_body_json(response)
}

fn unsupported(status: u16, param: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({"error":{
        "code":"unsupported_parameter", "param":param,
        "message":format!("{param} is not supported")
    }}))
}

async fn requests(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("recorded requests")
        .into_iter()
        .filter(|request| request.method == "POST")
        .map(|request| serde_json::from_slice(&request.body).expect("JSON request"))
        .collect()
}

fn assert_schema(format: &Value) {
    assert_eq!(format["type"], "json_schema");
    let schema = if format.get("json_schema").is_some() {
        &format["json_schema"]
    } else {
        format
    };
    assert_eq!(schema["strict"], true);
    assert!(schema["name"].as_str().is_some_and(|name| !name.is_empty()));
    assert_eq!(schema["schema"]["type"], "object");
    assert_eq!(schema["schema"]["additionalProperties"], false);
    assert_eq!(schema["schema"]["properties"]["approve"]["type"], "boolean");
    assert_eq!(schema["schema"]["properties"]["reason"]["type"], "string");
    let required = schema["schema"]["required"]
        .as_array()
        .expect("required fields");
    assert_eq!(required.len(), 4);
    assert!(
        required.contains(&json!("risk_level")) && required.contains(&json!("authorization_level"))
    );
    assert!(required.contains(&json!("approve")) && required.contains(&json!("reason")));
}

#[tokio::test]
async fn compatible_schema_and_reasoning_reach_the_real_review_path() {
    for (text, expected) in [
        (APPROVE, ReviewVerdict::Approve),
        (
            DENY,
            ReviewVerdict::Deny {
                reason: "unsafe effects".into(),
            },
        ),
    ] {
        let server = server(false).await;
        Mock::given(method("POST"))
            .respond_with(completion(text))
            .expect(1)
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), false, Duration::from_secs(2));
        assert_eq!(
            QuickModelReviewer::new(harness.model)
                .review("run-review", "pwd", "inspect directory")
                .await,
            Ok(expected)
        );
        let requests = requests(&server).await;
        assert_eq!(requests.len(), 1);
        assert_schema(&requests[0]["response_format"]);
        assert_eq!(requests[0]["messages"].as_array().unwrap().len(), 2);
        assert!(
            requests[0]
                .get("tools")
                .is_none_or(|tools| tools == &json!([]))
        );
        assert_eq!(requests[0]["model"], MODEL);
    }
}

#[tokio::test]
async fn unsupported_format_falls_back_once_with_identical_input_and_settings() {
    for status in [400, 422] {
        let server = server(false).await;
        Mock::given(method("POST"))
            .and(|request: &wiremock::Request| {
                request
                    .body_json::<Value>()
                    .unwrap()
                    .get("response_format")
                    .is_some()
            })
            .respond_with(unsupported(status, "response_format"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(|request: &wiremock::Request| {
                request
                    .body_json::<Value>()
                    .unwrap()
                    .get("response_format")
                    .is_none()
            })
            .respond_with(completion(APPROVE))
            .expect(1)
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), false, Duration::from_secs(2));
        assert_eq!(
            QuickModelReviewer::new(harness.model)
                .review("run-review", "pwd", "inspect directory")
                .await,
            Ok(ReviewVerdict::Approve)
        );
        let requests = requests(&server).await;
        assert_eq!(requests.len(), 2);
        assert_schema(&requests[0]["response_format"]);
        let mut original = requests[0].clone();
        original.as_object_mut().unwrap().remove("response_format");
        assert_eq!(
            original, requests[1],
            "only the unsupported format may change"
        );
        assert_eq!(requests[1]["temperature"], 0.25);
        assert_eq!(requests[1]["max_tokens"], 321);
    }
}

fn client_unavailable() -> ResponseTemplate {
    ResponseTemplate::new(400)
        .set_body_json(json!({"error":{"message":"model not available for client"}}))
}

async fn mount_review_candidates(server: &MockServer, fallback: ResponseTemplate) {
    for (key, response) in [("key-0", client_unavailable()), ("key-1", fallback)] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", format!("Bearer {key}")))
            .respond_with(response)
            .expect(1)
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn http_400_falls_back_to_next_candidate_and_approves() {
    let server = server(false).await;
    mount_review_candidates(&server, completion(APPROVE)).await;
    let harness = harness_fallback(&server.uri(), Duration::from_secs(2));
    assert_eq!(
        QuickModelReviewer::new(harness.model)
            .review("run-review", "pwd", "inspect directory")
            .await,
        Ok(ReviewVerdict::Approve)
    );
    let requests = requests(&server).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["messages"], requests[1]["messages"]);
    assert_eq!(
        requests[0]["response_format"],
        requests[1]["response_format"]
    );
    for request in &requests {
        assert_schema(&request["response_format"]);
        let messages = request["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[0]["content"],
            runtime::escalation_review::REVIEW_INSTRUCTION
        );
        let content = messages[1]["content"].as_str().expect("review payload");
        let payload: Value = serde_json::from_str(content).unwrap();
        assert_eq!(
            payload,
            json!({"command":"pwd","justification":"inspect directory"})
        );
        assert_eq!(request["temperature"], 0.25);
        assert_eq!(request["max_tokens"], 321);
        assert!(request.get("tools").is_none_or(|tools| tools == &json!([])));
    }
}

#[tokio::test]
async fn http_400_then_valid_deny_is_final_no_retry() {
    let server = server(false).await;
    mount_review_candidates(&server, completion(DENY)).await;
    let harness = harness_fallback(&server.uri(), Duration::from_secs(2));
    assert_eq!(
        QuickModelReviewer::new(harness.model)
            .review("run-review", "pwd", "inspect directory")
            .await,
        Ok(ReviewVerdict::Deny {
            reason: "unsafe effects".into(),
        })
    );
    assert_eq!(requests(&server).await.len(), 2);
}

#[tokio::test]
async fn http_400_on_all_candidates_fails_closed() {
    let server = server(false).await;
    mount_review_candidates(&server, client_unavailable()).await;
    let harness = harness_fallback(&server.uri(), Duration::from_secs(2));
    assert_eq!(
        QuickModelReviewer::new(harness.model)
            .review("run-review", "pwd", "inspect directory")
            .await,
        Err(ReviewError::Model)
    );
    let requests = requests(&server).await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_schema(&request["response_format"]);
    }
}

#[tokio::test]
async fn invalid_verdict_on_fallback_candidate_is_final() {
    let server = server(false).await;
    mount_review_candidates(&server, completion("```json\n{\"approve\":true}\n```")).await;
    let harness = harness_fallback(&server.uri(), Duration::from_secs(2));
    assert_eq!(
        QuickModelReviewer::new(harness.model)
            .review("run-review", "pwd", "inspect directory")
            .await,
        Err(ReviewError::InvalidVerdict)
    );
    assert_eq!(requests(&server).await.len(), 2);
}

#[tokio::test]
async fn structured_unsupported_400_keeps_same_provider_downgrade_precedence() {
    let server = server(false).await;
    for structured in [true, false] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer key-0"))
            .and(move |request: &wiremock::Request| {
                request
                    .body_json::<Value>()
                    .unwrap()
                    .get("response_format")
                    .is_some()
                    == structured
            })
            .respond_with(if structured {
                unsupported(400, "response_format")
            } else {
                completion(APPROVE)
            })
            .expect(1)
            .mount(&server)
            .await;
    }
    let harness = harness_fallback(&server.uri(), Duration::from_secs(2));
    assert_eq!(
        QuickModelReviewer::new(harness.model)
            .review("run-review", "pwd", "inspect directory")
            .await,
        Ok(ReviewVerdict::Approve)
    );
    let captured = server.received_requests().await.expect("recorded requests");
    let posts: Vec<_> = captured
        .iter()
        .filter(|request| request.method == "POST")
        .collect();
    assert_eq!(posts.len(), 2);
    assert!(
        posts
            .iter()
            .all(|request| request.headers["authorization"] == "Bearer key-0")
    );
    let requests = requests(&server).await;
    assert_schema(&requests[0]["response_format"]);
    let mut original = requests[0].clone();
    original.as_object_mut().unwrap().remove("response_format");
    assert_eq!(
        original, requests[1],
        "only the unsupported format may change"
    );
}

#[tokio::test]
async fn other_http_failures_never_remove_structured_output() {
    for (status, body) in [
        (401, json!({"error":{"message":"unauthorized"}})),
        (429, json!({"error":{"message":"rate limited"}})),
        (
            500,
            json!({"error":{"message":"response_format is not supported"}}),
        ),
        (
            400,
            json!({"error":{"code":"invalid_json_schema", "param":"response_format", "message":"Invalid schema: unsupported property type"}}),
        ),
        (
            422,
            json!({"error":{"param":"messages", "message":"messages is not supported"}}),
        ),
    ] {
        let server = server(false).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), false, Duration::from_secs(2));
        assert_eq!(
            QuickModelReviewer::new(harness.model)
                .review("run-review", "pwd", "inspect directory")
                .await,
            Err(ReviewError::Model),
            "HTTP {status}"
        );
        let requests = requests(&server).await;
        assert!(!requests.is_empty());
        // Transport/routing retries retain the schema; only capability rejection permits legacy.
        assert!(
            requests
                .iter()
                .all(|request| request.get("response_format").is_some())
        );
    }
}

#[tokio::test]
async fn transport_timeout_never_retries_in_legacy_mode() {
    let server = server(false).await;
    Mock::given(method("POST"))
        .respond_with(completion(APPROVE).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let harness = harness(&server.uri(), false, Duration::from_millis(30));
    assert_eq!(
        QuickModelReviewer::new(harness.model)
            .review("run-review", "pwd", "inspect directory")
            .await,
        Err(ReviewError::Model)
    );
    let requests = requests(&server).await;
    assert!(!requests.is_empty());
    assert!(
        requests
            .iter()
            .all(|request| request.get("response_format").is_some())
    );
}

#[tokio::test]
async fn invalid_verdict_or_denial_is_final_in_both_response_modes() {
    for legacy in [false, true] {
        for (text, expected) in [
            (
                DENY,
                Ok(ReviewVerdict::Deny {
                    reason: "unsafe effects".into(),
                }),
            ),
            (
                "```json\n{\"approve\":true}\n```",
                Err(ReviewError::InvalidVerdict),
            ),
            (
                r#"{"approve":true,"reason":"safe","extra":"ignore me"}"#,
                Err(ReviewError::InvalidVerdict),
            ),
        ] {
            let server = server(false).await;
            Mock::given(method("POST"))
                .respond_with(move |request: &wiremock::Request| {
                    if legacy
                        && request
                            .body_json::<Value>()
                            .unwrap()
                            .get("response_format")
                            .is_some()
                    {
                        unsupported(400, "response_format")
                    } else {
                        completion(text)
                    }
                })
                .expect(if legacy { 2 } else { 1 })
                .mount(&server)
                .await;
            let harness = harness(&server.uri(), false, Duration::from_secs(2));
            assert_eq!(
                QuickModelReviewer::new(harness.model)
                    .review("run-review", "pwd", "inspect directory")
                    .await,
                expected,
                "legacy={legacy}, text={text}"
            );
            assert_eq!(requests(&server).await.len(), if legacy { 2 } else { 1 });
        }
    }
}

fn codex_completion() -> ResponseTemplate {
    let events = [
        json!({"type":"response.reasoning_summary_text.delta","delta":"Consider effects."}),
        json!({"type":"response.output_text.delta","item_id":"review","content_index":0,"delta":APPROVE}),
        json!({"type":"response.completed","response":{"id":"review","status":"completed","usage":{"input_tokens":10,"output_tokens":3}}}),
    ];
    let body = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

#[tokio::test]
async fn codex_sse_review_uses_text_format_and_falls_back_without_changing_affinity() {
    for legacy in [false, true] {
        let server = server(true).await;
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                if legacy
                    && request
                        .body_json::<Value>()
                        .unwrap()
                        .pointer("/text/format")
                        .is_some()
                {
                    unsupported(400, "text.format")
                } else {
                    codex_completion()
                }
            })
            .expect(if legacy { 2 } else { 1 })
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), true, Duration::from_secs(2));
        assert_eq!(
            QuickModelReviewer::new(harness.model)
                .review("run-review", "pwd", "inspect directory")
                .await,
            Ok(ReviewVerdict::Approve)
        );
        let requests = requests(&server).await;
        assert_schema(&requests[0]["text"]["format"]);
        // Codex sends affinity as a header; compatibility retries must reuse it.
        let captured = server.received_requests().await.expect("recorded headers");
        let sessions = captured
            .iter()
            .filter(|request| request.method == "POST")
            .map(|request| request.headers["session-id"].to_str().unwrap())
            .collect::<Vec<_>>();
        assert!(!sessions[0].is_empty());
        assert!(sessions.iter().all(|session| *session == sessions[0]));
        if legacy {
            let mut original = requests[0].clone();
            original.as_object_mut().unwrap().remove("text");
            assert_eq!(original, requests[1]);
        }
    }
}

fn message(role: MessageRole, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

#[tokio::test]
async fn independent_review_preserves_the_normal_turn_prefix_and_cached_input() {
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("first", MODEL, ["first response"]),
        ScriptedResponse::text_stream("review", MODEL, [APPROVE]),
        ScriptedResponse::text_stream("second", MODEL, ["second response"]),
    ]);
    let harness = harness(&mock.base_url(), false, Duration::from_secs(2));
    let invocation = AgentInvocationContext {
        run_id: "normal-run".into(),
        category: None,
        model_preference: None,
    };
    let tools = [ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
    }];
    let mut history = vec![
        message(MessageRole::System, "Stable normal-turn instructions"),
        message(MessageRole::User, &"Inspect the project. ".repeat(200)),
    ];
    let bus = EventBus::new(64);
    let first = harness
        .model
        .complete_streaming(&invocation, Role::Worker, &history, &tools, &bus)
        .await
        .expect("first normal turn");
    assert_eq!(first.usage.cache_read_tokens, 0);
    assert_eq!(
        QuickModelReviewer::new(harness.model.clone())
            .review("normal-run", "pwd", "inspect directory")
            .await,
        Ok(ReviewVerdict::Approve)
    );
    history.push(first.message);
    history.push(message(MessageRole::User, "Continue inspection."));
    let second = harness
        .model
        .complete_streaming(&invocation, Role::Worker, &history, &tools, &bus)
        .await
        .expect("second normal turn");
    // The mock derives usage from actual input bytes, retaining all past prefixes.
    assert!(first.usage.input_tokens > 0);
    assert!(second.usage.cache_read_tokens >= first.usage.input_tokens);
    assert!(second.usage.cache_read_tokens < second.usage.input_tokens);
    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect();
    assert_eq!(requests.len(), 3);
    assert_append_only(CacheProtocol::OpenAi, &requests[0], &requests[2])
        .expect("normal history and settings stay stable across review");
    assert!(requests[0].get("response_format").is_none());
    assert_schema(&requests[1]["response_format"]);
    assert_eq!(requests[1]["messages"].as_array().unwrap().len(), 2);
    assert!(
        !requests[1]
            .to_string()
            .contains("Stable normal-turn instructions")
    );
    assert!(requests[2].get("response_format").is_none());
    assert_eq!(mock.remaining_scripts(), 0);
}

#[tokio::test]
async fn one_review_deadline_covers_structured_and_legacy_attempts() {
    let server = server(false).await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let response = if request
                .body_json::<Value>()
                .unwrap()
                .get("response_format")
                .is_some()
            {
                unsupported(400, "response_format")
            } else {
                completion(APPROVE)
            };
            response.set_delay(Duration::from_millis(300))
        })
        .expect(2)
        .mount(&server)
        .await;
    let harness = harness(&server.uri(), false, Duration::from_secs(2));
    assert_eq!(
        QuickModelReviewer::new(harness.model)
            .with_timeout(Duration::from_millis(500))
            .review("run-review", "pwd", "inspect directory")
            .await,
        Err(ReviewError::Timeout)
    );
    let requests = requests(&server).await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("response_format").is_some());
    assert!(requests[1].get("response_format").is_none());
}
