use std::sync::{Arc, Mutex};

use event_bus::{EventBus, EventKind, ProviderEvent};
use futures_util::FutureExt;
use providers::{HostedWebSearchError, ProviderAuth, ProviderError};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::support::*;

#[tokio::test]
async fn completed_usage_survives_missing_search_malformed_content_and_server_tool_errors() {
    let mut malformed_text = message();
    malformed_text["text"] = json!({"private":"invalid"});
    let mut invalid_url = results("srv_one");
    invalid_url["content"][0]["url"] = json!("javascript:private");
    let mut truncated = success();
    truncated["stop_reason"] = json!("max_tokens");
    let mut cases = vec![
        completed(json!([message()])),
        completed(json!([results("srv_one"), message()])),
        completed(json!([call("srv_one"), message()])),
        completed(json!([
            call("srv_one"),
            results("srv_one"),
            results("srv_one")
        ])),
        completed(json!([
            call("srv_one"),
            results("srv_one"),
            call("srv_pending")
        ])),
        completed(json!([call("srv_one"), malformed_text, results("srv_one")])),
        completed(json!([call("srv_one"), invalid_url])),
        completed(json!({"private":"not an array"})),
        truncated,
    ];
    for code in [
        "invalid_input",
        "too_many_requests",
        "max_uses_exceeded",
        "unavailable",
        "query_too_long",
        "private_unknown",
    ] {
        cases.push(completed(json!([call("srv_one"), {
            "type":"web_search_tool_result", "tool_use_id":"srv_one",
            "content":{"type":"web_search_tool_result_error", "error_code":code, "message":"private upstream body"}
        }])));
    }
    for oauth in [false, true] {
        for body in &cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let bus = Arc::new(EventBus::new(16));
            let mut receiver = bus.subscribe();
            let client = client(&server, oauth, Some(bus));
            let mut request = request();
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let observed = recorded.clone();
            request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
            let error = client
                .hosted_web_search()
                .unwrap()
                .search(&ProviderAuth::new("private-key"), &request)
                .await
                .unwrap_err();
            assert!(matches!(error, HostedWebSearchError::Provider(_)));
            match body["content"][1]["content"]["error_code"].as_str() {
                Some("unavailable") => assert!(matches!(
                    error,
                    HostedWebSearchError::Provider(ProviderError::Http { status: 503, .. })
                )),
                Some("too_many_requests") => assert!(matches!(
                    error,
                    HostedWebSearchError::Provider(ProviderError::RateLimited { .. })
                )),
                Some(_) => assert!(matches!(
                    error,
                    HostedWebSearchError::Provider(ProviderError::Request(_))
                )),
                None => {}
            }
            assert!(!format!("{error:?} {error}").contains("private"));
            assert_eq!(*recorded.lock().unwrap(), [usage()]);
            let (mut accounted, mut completed) = (0, 0);
            while let Some(event) = receiver.recv().now_or_never() {
                match event.unwrap().kind {
                    EventKind::Usage(_) => accounted += 1,
                    EventKind::Provider(ProviderEvent::RequestCompleted { .. }) => completed += 1,
                    EventKind::Provider(ProviderEvent::RequestFailed { .. }) => {
                        panic!("completed usage owns terminal event")
                    }
                    _ => {}
                }
            }
            assert_eq!((accounted, completed), (1, 1));
        }
    }
}

#[tokio::test]
async fn incomplete_or_invalid_usage_is_not_accounted_and_continuations_are_bounded() {
    let mut invalid_usage = success();
    invalid_usage["usage"]["input_tokens"] = json!("private-invalid");
    let mut unfinished = success();
    unfinished["stop_reason"] = Value::Null;
    let mut missing_usage = success();
    missing_usage.as_object_mut().unwrap().remove("usage");
    let mut wrong_type = success();
    wrong_type["type"] = json!("private-error");
    for body in [invalid_usage, unfinished, missing_usage, wrong_type] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server, false, None);
        let mut request = request();
        request.usage_sink = Some(Arc::new(|_| {
            panic!("incomplete usage must not be accounted")
        }));
        let error = client
            .hosted_web_search()
            .unwrap()
            .search(&ProviderAuth::new("private-key"), &request)
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("private"));
    }
    let server = MockServer::start().await;
    let mut body = completed(json!([]));
    body["stop_reason"] = json!("pause_turn");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(3)
        .mount(&server)
        .await;
    let client = client(&server, true, None);
    let mut request = request();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let observed = recorded.clone();
    request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
    let error = client
        .hosted_web_search()
        .unwrap()
        .search(&ProviderAuth::new("private-key"), &request)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("continuation limit"));
    assert_eq!(*recorded.lock().unwrap(), [usage(); 3]);
}

#[tokio::test]
async fn only_explicit_search_tool_rejection_permits_fallback() {
    let unsupported = json!({"type":"error","error":{"type":"invalid_request_error","message":"Tool web_search_20250305 is not supported private-body"}});
    let mut cases = vec![
        (400, unsupported.clone(), true),
        (
            400,
            json!({"error":{"type":"invalid_request_error","message":"Unknown tool type: web_search_20250305"}}),
            true,
        ),
        (
            400,
            json!({"error":{"type":"invalid_request_error","message":"Unknown tool type: web_search_20260209"}}),
            false,
        ),
        (
            400,
            json!({"error":{"type":"authentication_error","message":"web_search is not supported private-body"}}),
            false,
        ),
        (
            400,
            json!({"error":{"type":"invalid_request_error","param":"model","message":"web_search is not supported"}}),
            false,
        ),
        (
            400,
            json!({"error":{"type":"invalid_request_error","message":"Unsupported tool_choice for web_search"}}),
            false,
        ),
        (
            400,
            json!({"error":{"type":"invalid_request_error","code":"model_not_found","message":"web_search is not supported"}}),
            false,
        ),
        (200, unsupported.clone(), false),
    ];
    for status in [401, 403, 404, 422, 429, 500, 501, 503] {
        cases.push((status, unsupported.clone(), false));
    }
    for oauth in [false, true] {
        for (status, body, fallback) in &cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(*status)
                        .insert_header("retry-after", "7")
                        .set_body_json(body),
                )
                .expect(1)
                .mount(&server)
                .await;
            let error = client(&server, oauth, None)
                .hosted_web_search()
                .unwrap()
                .search(&ProviderAuth::new("private-key"), &request())
                .await
                .unwrap_err();
            assert_eq!(
                matches!(error, HostedWebSearchError::Unsupported),
                *fallback,
                "{status}: {error:?}"
            );
            assert!(!format!("{error:?} {error}").contains("private"));
            if let HostedWebSearchError::Provider(error) = error {
                if *status != 200 {
                    assert_eq!(error.status(), Some(*status));
                }
                if *status == 429 {
                    assert!(
                        matches!(error, ProviderError::RateLimited { retry_after: Some(value) } if value.as_secs() == 7)
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn malformed_and_oversized_response_bodies_are_sanitized() {
    for oauth in [false, true] {
        for body in [
            "private-invalid-json".to_owned(),
            format!("{{\"private\":\"{}\"}}", "x".repeat(1024 * 1024)),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
            let error = client(&server, oauth, None)
                .hosted_web_search()
                .unwrap()
                .search(&ProviderAuth::new("private-key"), &request())
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                HostedWebSearchError::Provider(ProviderError::InvalidJson { .. })
            ));
            assert!(!format!("{error:?} {error}").contains("private"));
        }
    }
}
