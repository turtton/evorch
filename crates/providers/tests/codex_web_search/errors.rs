use std::sync::{Arc, Mutex};

use event_bus::{EventBus, EventKind, ProviderEvent};
use futures_util::FutureExt;
use providers::{HostedWebSearch, HostedWebSearchError, ProviderAuth, ProviderError};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::support::*;

#[tokio::test]
async fn only_explicit_tool_unavailability_allows_fallback() {
    let unsupported = json!({"error":{"code":"unsupported_tool","message":"Tool 'web_search' is not supported secret-body"}});
    let mut cases = vec![
        (400, unsupported.to_string(), true),
        (400, json!({"error":{"message":"Unknown tool type: web_search","param":"tools[0].type"}}).to_string(), true),
        (400, json!({"error":{"message":"web_search tool is not supported","param":"model"}}).to_string(), false),
        (400, json!({"error":{"code":"model_not_found","message":"web_search is not supported for missing model"}}).to_string(), false),
        (400, json!({"error":{"message":"Unsupported model"},"request":{"tools":[{"type":"web_search"}]}}).to_string(), false),
        (400, json!({"error":{"message":"Unsupported web_search option search_context_size"}}).to_string(), false),
        (400, json!({"error":{"message":"Unsupported tool_choice for web_search","param":"tool_choice","code":"unsupported_value"}}).to_string(), false),
        (400, json!({"error":{"message":"Unknown tool: web_search_preview"}}).to_string(), false),
        (400, json!({"error":{"type":"authentication_error","message":"web_search is not supported"}}).to_string(), false),
        (400, "secret-body invalid JSON".into(), false),
        (200, sse([json!({"type":"response.failed", "response":{"error":unsupported["error"]}})]), true),
        (200, sse([json!({"type":"error", "code":"unsupported_tool", "message":"Unknown tool: web_search"})]), true),
        (200, sse([json!({"type":"response.failed", "response":{"error":{"code":"invalid_api_key", "message":"web_search is not supported secret-body"}}})]), false),
        (200, sse([json!({"type":"error", "status_code":403,"error":{"code":"unsupported_tool", "message":"Unknown tool: web_search"}})]), false),
    ];
    for status in [401, 403, 404, 408, 422, 429, 500, 501, 503] {
        cases.push((status, unsupported.to_string(), false));
    }
    for (status, body, fallback) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let result = client(&server, store("secret-access", "account", u64::MAX), None)
            .search(&ProviderAuth::new("unused"), &request())
            .await;
        assert_eq!(
            matches!(result, Err(HostedWebSearchError::Unsupported)),
            fallback,
            "{status}: {result:?}"
        );
        let error = result.unwrap_err();
        assert!(!format!("{error:?} {error}").contains("secret-"));
        if let HostedWebSearchError::Provider(error) = error
            && status != 200
        {
            assert_eq!(error.status(), Some(status));
        }
    }
}

#[tokio::test]
async fn incomplete_invalid_and_missing_search_responses_are_errors() {
    let mut invalid_usage = completed(json!([search_call(), message()]));
    invalid_usage["response"]["usage"]["input_tokens"] = json!("secret-invalid-usage");
    let mut unfinished = search_call();
    unfinished["status"] = json!("in_progress");
    for (body, accounted) in [
        ("".into(), false),
        ("data: [DONE]\n\n".into(), false),
        ("data: secret-invalid-json\n\n".into(), false),
        (
            sse([json!({"type":"response.output_text.delta","delta":"looks like search"})]),
            false,
        ),
        (
            sse([
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"secret"}}}),
            ]),
            false,
        ),
        (
            sse([
                json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"secret-body"}}}),
            ]),
            false,
        ),
        (sse([invalid_usage]), false),
        (sse([completed(json!([message()]))]), true),
        (sse([completed(json!([unfinished, message()]))]), true),
        (
            sse([
                json!({"type":"response.output_item.added","item":search_call()}),
                completed(json!([message()])),
            ]),
            true,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let bus = Arc::new(EventBus::new(32));
        let mut receiver = bus.subscribe();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut request = request();
        let observed = calls.clone();
        request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
        let result = client(
            &server,
            store("secret-access", "account", u64::MAX),
            Some(bus),
        )
        .search(&ProviderAuth::new("unused"), &request)
        .await;
        assert!(
            matches!(
                result,
                Err(HostedWebSearchError::Provider(
                    ProviderError::InvalidSse { .. }
                ))
            ),
            "{result:?}"
        );
        assert!(!result.unwrap_err().to_string().contains("secret"));
        assert_eq!(calls.lock().unwrap().len(), usize::from(accounted));
        let mut terminal_count = 0;
        let mut usage_count = 0;
        while let Some(event) = receiver.recv().now_or_never() {
            match event.unwrap().kind {
                EventKind::Usage(_) => usage_count += 1,
                EventKind::Provider(
                    ProviderEvent::RequestCompleted { .. } | ProviderEvent::RequestFailed { .. },
                ) => terminal_count += 1,
                _ => {}
            }
        }
        assert_eq!(terminal_count, 1);
        assert_eq!(usage_count, usize::from(accounted));
    }
}

#[tokio::test]
async fn refresh_failure_is_sanitized_and_never_becomes_search_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("secret-refresh-token"))
        .expect(1)
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let result = client(&server, store("expired", "account", 0), Some(bus))
        .search(&ProviderAuth::new("unused"), &request())
        .await;
    assert!(matches!(
        result,
        Err(HostedWebSearchError::Provider(ProviderError::Http {
            status: 401,
            ..
        }))
    ));
    assert!(!format!("{result:?}").contains("secret-refresh-token"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    while let Some(event) = receiver.recv().now_or_never() {
        assert!(!matches!(
            event.unwrap().kind,
            EventKind::Usage(_) | EventKind::Provider(ProviderEvent::RequestStarted { .. })
        ));
    }
}
