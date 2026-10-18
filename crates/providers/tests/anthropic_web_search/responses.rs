use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use event_bus::{CacheComparison, EventBus, EventKind, ProviderEvent, RequestPurpose};
use futures_util::FutureExt;
use providers::ProviderAuth;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::support::*;

#[tokio::test]
async fn api_and_subscription_use_native_search_and_account_completed_usage_once() {
    for oauth in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(success()))
            .expect(1)
            .mount(&server)
            .await;
        let bus = Arc::new(EventBus::new(32));
        let mut receiver = bus.subscribe();
        let client = client(&server, oauth, Some(bus));
        let capability = client.hosted_web_search().unwrap();
        assert_eq!(
            capability.provider_name(),
            if oauth { "claude" } else { "anthropic" }
        );
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let observed = recorded.clone();
        let mut request = request();
        request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
        let result = capability
            .search(&ProviderAuth::new("secret-api-key"), &request)
            .await
            .unwrap();
        assert_eq!(result.text, "Rust release details.");
        assert_eq!(result.response_id.as_deref(), Some("msg_search"));
        assert_eq!(result.usage, usage());
        assert_eq!(*recorded.lock().unwrap(), [usage()]);
        assert_eq!(result.citations.len(), 2);
        assert_eq!(result.citations[0].title, "Rust release");
        assert_eq!(result.citations[1].url, "https://docs.rs");
        assert!(!format!("{result:?}").contains("private"));
        let requests = server.received_requests().await.unwrap();
        let captured = &requests[0];
        let body: Value = captured.body_json().unwrap();
        assert_eq!(
            body["tools"],
            json!([{"type":"web_search_20250305","name":"web_search","max_uses":8}])
        );
        assert_eq!(body["model"], request.model);
        assert_eq!(
            body["messages"],
            json!([{"role":"user","content":[{"type":"text","text":request.query}]}])
        );
        assert_eq!(body["stream"], false);
        assert!(body.get("tool_choice").is_none());
        assert!(!body.to_string().contains("cache_control"));
        if oauth {
            assert_eq!(
                captured.headers["authorization"],
                "Bearer secret-subscription-token"
            );
            assert!(captured.headers.get("x-api-key").is_none());
            assert_eq!(captured.url.query(), Some("beta=true"));
            assert_eq!(captured.headers["x-claude-code-request-class"], "auxiliary");
            let identity: Value =
                serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
            assert_eq!(identity["account_uuid"], "secret-account");
            assert_eq!(
                identity["session_id"].as_str().unwrap(),
                captured.headers["x-claude-code-session-id"]
                    .to_str()
                    .unwrap()
            );
            mock_openai::claude_contract::claude_model_input(&captured.body).unwrap();
        } else {
            assert_eq!(captured.headers["x-api-key"], "secret-api-key");
            assert!(captured.headers.get("authorization").is_none());
            assert!(body.get("metadata").is_none());
            assert!(captured.url.query().is_none());
        }
        let (mut usage_count, mut terminals) = (0, 0);
        while let Some(event) = receiver.recv().now_or_never() {
            match event.unwrap().kind {
                EventKind::Usage(_) => usage_count += 1,
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    purpose,
                    provider,
                    profile,
                    ..
                }) => {
                    terminals += 1;
                    assert_eq!(purpose, Some(RequestPurpose::WebSearch));
                    assert_eq!(provider, capability.provider_name());
                    assert_eq!(profile.as_deref(), Some("selected-profile"));
                }
                EventKind::Provider(
                    ProviderEvent::RequestFailed { .. } | ProviderEvent::CacheReuseObserved { .. },
                ) => panic!("search failed or touched cache baseline"),
                _ => {}
            }
        }
        assert_eq!((usage_count, terminals), (1, 1));
    }
}

#[tokio::test]
async fn pause_turn_preserves_opaque_blocks_and_sums_each_response_usage() {
    for oauth in [false, true] {
        let server = MockServer::start().await;
        let paused_content = json!([
            {"type":"thinking", "thinking":"private reasoning", "signature":"signed-private"},
            call("srv_first"), results("srv_first"), call("srv_second")
        ]);
        let mut paused = completed(paused_content.clone());
        paused["stop_reason"] = json!("pause_turn");
        paused["id"] = json!("msg_paused");
        let finished = completed(json!([results("srv_second"), message()]));
        let sequence = AtomicUsize::new(0);
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                if sequence.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(200)
                        .insert_header("request-id", "req_paused")
                        .set_body_json(&paused)
                } else {
                    ResponseTemplate::new(200).set_body_json(&finished)
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let client = client(&server, oauth, None);
        let mut request = request();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let observed = recorded.clone();
        request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
        let result = client
            .hosted_web_search()
            .unwrap()
            .search(&ProviderAuth::new("key"), &request)
            .await
            .unwrap();
        assert_eq!(result.usage.input_tokens, 84);
        assert_eq!(result.usage.output_tokens, 14);
        assert_eq!(result.usage.cache_read_tokens, 60);
        assert_eq!(result.usage.cache_write_tokens, 4);
        assert_eq!(*recorded.lock().unwrap(), [usage(), usage()]);
        assert_eq!(result.text, "Rust release details.");
        assert!(!format!("{result:?}").contains("private"));
        let captured = server.received_requests().await.unwrap();
        let first: Value = captured[0].body_json().unwrap();
        let second: Value = captured[1].body_json().unwrap();
        assert_eq!(second["messages"][0], first["messages"][0]);
        assert_eq!(
            second["messages"][1],
            json!({"role":"assistant","content":paused_content})
        );
        assert_eq!(second["messages"].as_array().unwrap().len(), 2);
        assert_eq!(first["tools"], second["tools"]);
        if oauth {
            mock_openai::claude_contract::claude_model_input(&captured[1].body).unwrap();
            assert_eq!(second["diagnostics"]["previous_message_id"], "msg_paused");
        }
    }
}

#[tokio::test]
async fn zero_results_are_successful_and_citation_limits_are_bounded() {
    let server = MockServer::start().await;
    let mut empty = results("srv_one");
    empty["content"] = json!([]);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completed(json!([
            call("srv_one"), empty, {"type":"text", "text":"No matching sources."}
        ]))))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let client = client(&server, false, None);
    let capability = client.hosted_web_search().unwrap();
    let auth = ProviderAuth::new("key");
    let result = capability.search(&auth, &request()).await.unwrap();
    assert!(result.citations.is_empty());
    assert_eq!(result.text, "No matching sources.");
    let mut many = results("srv_one");
    many["content"] = (0..25)
        .map(|i| json!({"type":"web_search_result","url":format!("https://example.com/{i}")}))
        .collect();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completed(json!([call("srv_one"), many]))),
        )
        .expect(5)
        .mount(&server)
        .await;
    for (limit, count) in [
        (None, 10),
        (Some(0), 0),
        (Some(1), 1),
        (Some(3), 3),
        (Some(u32::MAX), 10),
    ] {
        let mut request = request();
        request.max_results = limit;
        assert_eq!(
            capability
                .search(&auth, &request)
                .await
                .unwrap()
                .citations
                .len(),
            count
        );
    }
}

#[tokio::test]
async fn search_isolated_from_chat_cache_and_claude_response_correlation() {
    for oauth in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &wiremock::Request| {
                let body: Value = request.body_json().unwrap();
                let (id, result) = if body["tools"][0]["type"] == "web_search_20250305" {
                    ("req_search", success())
                } else {
                    let mut result = completed(json!([{"type":"text","text":"ordinary reply"}]));
                    result["id"] = json!("msg_chat");
                    ("req_chat", result)
                };
                ResponseTemplate::new(200)
                    .insert_header("request-id", id)
                    .set_body_json(result)
            })
            .expect(3)
            .mount(&server)
            .await;
        let bus = Arc::new(EventBus::new(32));
        let mut receiver = bus.subscribe();
        let client = client(&server, oauth, Some(bus));
        let auth = ProviderAuth::new("key");
        client.send(&auth, &chat_request()).await.unwrap();
        client
            .hosted_web_search()
            .unwrap()
            .search(&auth, &request())
            .await
            .unwrap();
        client.send(&auth, &chat_request()).await.unwrap();
        let captured = server.received_requests().await.unwrap();
        if oauth {
            let first =
                mock_openai::claude_contract::claude_model_input(&captured[0].body).unwrap();
            let last = mock_openai::claude_contract::claude_model_input(&captured[2].body).unwrap();
            assert_eq!(first, last);
            let search: Value = captured[1].body_json().unwrap();
            let last: Value = captured[2].body_json().unwrap();
            assert!(search["diagnostics"]["previous_message_id"].is_null());
            assert_eq!(last["diagnostics"]["previous_message_id"], "msg_chat");
            assert!(
                last["system"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("cc_prev_req=req_chat;")
            );
        } else {
            assert_eq!(captured[0].body, captured[2].body);
        }
        let mut comparisons = Vec::new();
        while let Some(event) = receiver.recv().now_or_never() {
            if let EventKind::Provider(ProviderEvent::CacheReuseObserved { comparison, .. }) =
                event.unwrap().kind
            {
                comparisons.push(comparison);
            }
        }
        assert_eq!(comparisons.len(), 2);
        assert!(matches!(
            comparisons[1],
            CacheComparison::Compared {
                previous_cache_tokens: 32,
                ..
            }
        ));
    }
}
