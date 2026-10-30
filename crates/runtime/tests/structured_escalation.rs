//! Offline review contracts through runtime routing and the real provider HTTP adapters.

#[path = "structured_escalation/support.rs"]
mod support;

#[path = "support/mod.rs"]
mod runtime_support;

use std::time::Duration;

use event_bus::EventBus;
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::{ContentBlock, Message, Role as MessageRole, ToolSpec};
use runtime::escalation_review::{QuickModelReviewer, ReviewError, ReviewVerdict};
use runtime::{AgentInvocationContext, Role};
use serde_json::{Value, json};
use support::{harness, harness_fallback, harness_with_reviewer_model};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gpt-4o";
const REVIEW_MODEL: &str = "gpt-4o-review";
const APPROVE: &str = r#"{"approve":true,"reason":"safe inspection"}"#;
const DENY: &str = r#"{"approve":false,"reason":"unsafe effects"}"#;

async fn server(codex: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(if codex {
            json!({"models":[{"slug":MODEL},{"slug":REVIEW_MODEL}]})
        } else {
            json!({"data":[{"id":MODEL},{"id":REVIEW_MODEL}]})
        }))
        .mount(&server)
        .await;
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
    assert_eq!(required.len(), 5);
    assert!(required.contains(&json!("procedure_step")));
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
async fn shell_review_uses_reviewer_tool_execution_model_and_generation() {
    let server = server(false).await;
    Mock::given(method("POST"))
        .respond_with(completion(APPROVE))
        .expect(1)
        .mount(&server)
        .await;
    let harness = harness_with_reviewer_model(&server.uri(), Duration::from_secs(2));
    assert_eq!(
        QuickModelReviewer::new(harness.model)
            .review("run-review", "pwd", "inspect directory")
            .await,
        Ok(ReviewVerdict::Approve)
    );
    let captured = requests(&server).await;
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], REVIEW_MODEL);
    assert_eq!(captured[0]["temperature"], 0.25);
    assert_eq!(captured[0]["max_tokens"], 321);
    assert_schema(&captured[0]["response_format"]);
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
            json!({"command":"pwd","justification":"inspect directory","access":"host_unsandboxed"})
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

fn codex_completion(text: &str) -> ResponseTemplate {
    let events = [
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"item_id":"rs-review","delta":"Consider effects."}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs-review","summary":[{"type":"summary_text","text":"Consider effects."}],"encrypted_content":"dummy-encrypted-review"}}),
        json!({"type":"response.output_text.delta","output_index":1,"item_id":"review","content_index":0,"delta":text}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"review","role":"assistant","content":[{"type":"output_text","text":text}],"status":"completed"}}),
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
                    codex_completion(APPROVE)
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

#[tokio::test]
async fn codex_sse_file_review_accepts_encrypted_reasoning() {
    use runtime::escalation_review::SandboxEscalationGate;
    use std::sync::{Arc, Mutex};
    use tools::tools::shell_escalation::{
        EscalationDecision, FileAccessRequest, ShellEscalationGate,
    };

    let server = server(true).await;
    Mock::given(method("POST"))
        .respond_with(codex_completion(
            r#"{"approve":true,"reason":"safe read","risk_level":"low","authorization_level":"high"}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let harness = harness(&server.uri(), true, Duration::from_secs(3600));
    let gate = SandboxEscalationGate::new(
        Arc::new(Mutex::new((config::EscalationApproval::Auto, false))),
        Some(Arc::new(QuickModelReviewer::new(harness.model))),
        Arc::new(EventBus::new(32)),
    );
    let decision = gate
        .decide_file_access(
            &tools::ToolExecutionContext {
                run_id: "file-review".into(),
                ..Default::default()
            },
            &FileAccessRequest {
                tool: "read".into(),
                paths: vec!["/outside/docs.txt".into()],
                write: false,
                justification: "inspect requested documentation".into(),
                change: None,
            },
        )
        .await;
    assert!(matches!(decision, EscalationDecision::Approve));
    assert_schema(&requests(&server).await[0]["text"]["format"]);
}

#[tokio::test]
async fn codex_sse_web_review_accepts_encrypted_reasoning() {
    use event_bus::{AgentRunPhase, EventKind, LifecycleEvent, ToolEvent};
    use providers::FinishReason;
    use runtime::{AgentRuntime, RunConfig};
    use runtime_support::{ScriptedModel, text_response, tool_response};
    use std::sync::Arc;

    let server = server(true).await;
    Mock::given(method("POST"))
        .respond_with(codex_completion(
            r#"{"approve":true,"reason":"requested page","risk_level":"low","authorization_level":"high"}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let harness = harness(&server.uri(), true, Duration::from_secs(3600));
    let model = support::ReviewAdapterModel {
        reviewer: harness.model,
        conversation: ScriptedModel::new([
            // NetworkGuard rejects loopback after review; no external fetch is needed.
            Ok(tool_response(
                "web-review",
                "web_fetch",
                json!({"url":"http://127.0.0.1/"}),
            )),
            Ok(text_response("finished", FinishReason::Stop)),
        ]),
    };
    let bus = Arc::new(EventBus::new(128));
    let executor = tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    )
    .with_web_tools()
    .expect("web tools");
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(executor), Arc::new(model));
    let mut events = bus.subscribe();
    let run = runtime
        .delegate_chat(
            "web-review",
            Role::Orchestrator,
            "Read the requested page".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let mut observed = Vec::new();
    loop {
        let event = events.recv().await.expect("run event");
        let done = matches!(&event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id, to: AgentRunPhase::Done, ..
            }) if *run_id == run.to_string()
        );
        observed.push(event);
        if done {
            break;
        }
    }
    let audit = observed
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Diagnostic(diag) if diag.code == "tool_call_access" => {
                Some(serde_json::from_str::<Value>(&diag.detail).expect("audit"))
            }
            _ => None,
        })
        .expect("web review audit");
    assert_eq!(audit["decision"], "allow");
    assert_eq!(audit["reviewer"], "auto_review");
    assert!(!observed.iter().any(|event| matches!(
        event.kind,
        EventKind::Tool(ToolEvent::ApprovalRequested { .. })
    )));
    assert_schema(&requests(&server).await[0]["text"]["format"]);
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
        attempts: None,
        turn: None,
        run_id: "normal-run".into(),
        category: None,
        model_preference: None,
        purpose: Default::default(),
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

const CLEANUP_STEPS: &str = "After the merge, remove the task worktree.\n";
const CRITICAL_VERIFIED: &str = r#"{"approve":true,"reason":"cleanup the procedure defines","risk_level":"critical","authorization_level":"high","procedure_step":"verified"}"#;

/// One agent run that reads a skill, then has a shell command reviewed with it
/// in context. Returns the chat requests the agent itself sent, in order, and
/// the single review request.
async fn run_skill_review(typed: String, mentions: &[&str]) -> (Vec<Value>, Value) {
    use event_bus::AgentRunPhase;
    use runtime::skill::{SkillScope, discover_skills};
    use runtime::{AgentRuntime, RunConfig};
    use std::sync::Arc;

    let skills = tempfile::tempdir().expect("skills");
    std::fs::create_dir(skills.path().join("worktree-pr")).expect("skill dir");
    std::fs::write(
        skills.path().join("worktree-pr/SKILL.md"),
        format!("---\nname: worktree-pr\ndescription: worktree steps\n---\n{CLEANUP_STEPS}"),
    )
    .expect("skill");
    std::fs::create_dir(skills.path().join("extra")).expect("skill dir");
    std::fs::write(
        skills.path().join("extra/SKILL.md"),
        "---\nname: extra\ndescription: extra steps\n---\nExtra procedure steps.\n",
    )
    .expect("skill");
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::tool_call(
                "load",
                MODEL,
                0,
                "call-skill",
                "skill_load",
                [r#"{"name":"worktree-pr"}"#],
            ),
            ScriptedResponse::tool_call(
                "shell",
                MODEL,
                0,
                "call-shell",
                "shell",
                [
                    r#"{"command":"printf cleaned","sandbox_access":"unsandboxed","justification":"post-merge cleanup"}"#,
                ],
            ),
            ScriptedResponse::text_stream("review", MODEL, [CRITICAL_VERIFIED]),
            ScriptedResponse::text_stream("finish", MODEL, ["cleaned up"]),
        ],
        vec![MODEL.to_owned(), REVIEW_MODEL.to_owned()],
    );
    let harness = harness(&mock.base_url(), false, Duration::from_secs(5));
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime =
        AgentRuntime::new(bus, executor.clone(), harness.model.clone()).with_skills(Arc::new(
            discover_skills(&[(SkillScope::User, skills.path().to_path_buf())]),
        ));
    runtime.set_sandbox_escalation(config::EscalationApproval::Auto, false);
    executor.set_shell_escalation(
        runtime.shell_escalation_gate(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    );
    let run = runtime
        .delegate_chat(
            "skill-review",
            Role::Worker,
            typed,
            RunConfig {
                skill_mentions: mentions.iter().map(|name| (*name).to_owned()).collect(),
                ..RunConfig::default()
            },
        )
        .expect("start");
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let bodies: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect();
    assert_eq!(mock.remaining_scripts(), 0, "{bodies:#?}");
    let (reviews, turns): (Vec<_>, Vec<_>) = bodies
        .into_iter()
        .partition(|body| body.get("response_format").is_some());
    assert_eq!(reviews.len(), 1);
    (turns, reviews.into_iter().next().expect("review"))
}

fn assert_skill_review_keeps_the_prefix(turns: &[Value]) {
    assert_eq!(turns.len(), 3);
    for pair in turns.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1])
            .expect("a skill read and a review leave the sent prefix alone");
        assert_eq!(pair[0]["tools"], pair[1]["tools"]);
        assert_eq!(pair[0]["messages"][0], pair[1]["messages"][0]);
    }
    // The review's skill context is its own request, never part of the agent's.
    assert!(
        turns
            .iter()
            .all(|turn| !turn.to_string().contains("loaded_skills"))
    );
}

/// The text of the first user message a chat request carries.
fn first_user_text(turn: &Value) -> String {
    let message = turn["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "user")
        .expect("user message");
    match &message["content"] {
        Value::String(text) => text.clone(),
        parts => parts
            .as_array()
            .expect("content parts")
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect(),
    }
}

fn review_context(review: &Value) -> Value {
    let user = review["messages"][1]["content"]
        .as_str()
        .expect("review payload");
    let payload: Value = serde_json::from_str(user).expect("payload JSON");
    payload["context"].clone()
}

#[tokio::test]
async fn a_loaded_skill_reaches_the_review_and_the_normal_turns_keep_their_prefix() {
    let (turns, review) = run_skill_review("CI通ったらマージしていいよ".into(), &[]).await;
    assert_skill_review_keeps_the_prefix(&turns);
    let context = review_context(&review);
    assert_eq!(
        context["real_user_requests"][0]["text"],
        "CI通ったらマージしていいよ"
    );
    assert_eq!(context["loaded_skills"][0]["name"], "worktree-pr");
    assert_eq!(context["loaded_skills"][0]["loaded_by"], "skill_load");
    assert_eq!(context["loaded_skills"][0]["text"], CLEANUP_STEPS);
    // The verified critical step ran without asking the user.
    assert!(
        turns[2]["messages"]
            .to_string()
            .contains("exit_code: 0\\ncleaned")
    );
}

#[tokio::test]
async fn a_mention_reaches_the_review_apart_from_the_user_words_and_the_agent_sees_the_same_bytes()
{
    let typed = "CI通ったらマージしていいよ @extra";
    let (turns, review) = run_skill_review(typed.into(), &["extra"]).await;
    assert_skill_review_keeps_the_prefix(&turns);
    // The agent is sent the typed words and the block, exactly as a host that
    // expanded the mention itself would have sent them.
    let mut expected = typed.to_owned();
    runtime::skill::append_skill_attachment(&mut expected, "extra", "Extra procedure steps.\n");
    assert_eq!(first_user_text(&turns[0]), expected);
    let context = review_context(&review);
    assert_eq!(context["real_user_requests"][0]["text"], typed);
    let skills = context["loaded_skills"].as_array().expect("skills");
    let attached = skills
        .iter()
        .find(|skill| skill["name"] == "extra")
        .expect("attached skill");
    assert_eq!(attached["loaded_by"], "user_mention");
    assert_eq!(attached["trust"], "user-attached");
    assert_eq!(attached["scope"], "user");
    assert_eq!(attached["text"], "Extra procedure steps.");
    assert!(skills.iter().any(|skill| skill["name"] == "worktree-pr"));
}
