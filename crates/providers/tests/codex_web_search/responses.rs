use std::sync::{Arc, Mutex};

use event_bus::{CacheComparison, EventBus, EventKind, ProviderEvent};
use futures_util::FutureExt;
use providers::{HostedWebSearch, ProviderAuth, ProviderClient};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::support::*;

#[tokio::test]
async fn final_items_deduplicate_deltas_citations_sources_and_emit_usage_once() {
    for final_output in ["complete", "absent", "empty", "message_only"] {
        let server = MockServer::start().await;
        let mut complete = completed(json!([search_call(), message()]));
        match final_output {
            "absent" => {
                complete["response"]
                    .as_object_mut()
                    .unwrap()
                    .remove("output");
            }
            "empty" => complete["response"]["output"] = json!([]),
            "message_only" => complete["response"]["output"] = json!([message()]),
            _ => {}
        }
        let events = [
            json!({"type":"response.created","response":{"id":"resp-search"}}),
            json!({"type":"response.output_text.delta","delta":"Rust release details."}),
            json!({"type":"response.output_item.done","output_index":0,"item":search_call()}),
            json!({"type":"response.output_item.done","output_index":1,"item":message()}),
            complete.clone(),
            complete, // A repeated terminal frame is not a second billable call.
        ];
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse(events), "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let bus = Arc::new(EventBus::new(32));
        let mut receiver = bus.subscribe();
        let client = client(&server, store("token", "account", u64::MAX), Some(bus));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut request = request();
        let observed = calls.clone();
        request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
        let response = client
            .search(&ProviderAuth::new("unused"), &request)
            .await
            .unwrap();
        assert_eq!(response.response_id.as_deref(), Some("resp-search"));
        assert_eq!(response.text, "Rust release details.");
        assert_eq!(response.citations.len(), 2);
        assert_eq!(response.citations[0].title, "Rust release");
        assert_eq!(response.citations[1].url, "https://docs.rs");
        assert_eq!(response.usage, usage());
        assert_eq!(*calls.lock().unwrap(), [usage()]);
        let mut usage_events = 0;
        let mut completed_events = 0;
        while let Some(event) = receiver.recv().now_or_never() {
            match event.unwrap().kind {
                EventKind::Usage(_) => usage_events += 1,
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    run_id,
                    profile,
                    model,
                    input_tokens,
                    output_tokens,
                    reasoning_tokens,
                    purpose,
                    ..
                }) => {
                    completed_events += 1;
                    assert_eq!(run_id.as_deref(), Some("search-run"));
                    assert_eq!(profile.as_deref(), Some("selected-profile"));
                    assert_eq!(model, "selected-codex-model");
                    assert_eq!(purpose, Some(event_bus::RequestPurpose::WebSearch));
                    assert_eq!(
                        (input_tokens, output_tokens, reasoning_tokens),
                        (42, 7, Some(3))
                    );
                }
                EventKind::Provider(ProviderEvent::RequestFailed { .. }) => {
                    panic!("search succeeded")
                }
                EventKind::Provider(ProviderEvent::CacheReuseObserved { .. }) => {
                    panic!("search must not touch chat cache baseline")
                }
                _ => {}
            }
        }
        assert_eq!((usage_events, completed_events), (1, 1));
    }
}

#[tokio::test]
async fn source_count_obeys_requested_limit_and_global_bound() {
    let server = MockServer::start().await;
    let mut call = search_call();
    call["action"]["sources"] = (0..25)
        .map(|i| json!({"type":"url","url":format!("https://example.com/{i}")}))
        .collect();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse([completed(json!([call, message()]))]),
            "text/event-stream",
        ))
        .expect(5)
        .mount(&server)
        .await;
    let client = client(&server, store("token", "account", u64::MAX), None);
    for (limit, count) in [
        (None, 10),
        (Some(0), 0),
        (Some(1), 1),
        (Some(3), 3),
        (Some(u32::MAX), 10),
    ] {
        let mut request = request();
        request.max_results = limit;
        let response = client
            .search(&ProviderAuth::new("unused"), &request)
            .await
            .unwrap();
        assert_eq!(response.citations.len(), count);
    }
}

#[tokio::test]
async fn completed_usage_survives_invalid_content_and_malformed_trailing_sse() {
    let mut malformed_message = message();
    malformed_message["content"][0]["text"] = json!(42);
    let mut bad_url = message();
    bad_url["content"][0]["annotations"][0]["url"] = json!("javascript:secret");
    for (events, succeeds) in [
        (sse([completed(json!([message()]))]).into_bytes(), false),
        (
            sse([completed(json!([search_call(), malformed_message.clone()]))]).into_bytes(),
            false,
        ),
        (
            sse([
                json!({"type":"response.output_item.done","output_index":0,"item":search_call()}),
                json!({"type":"response.output_item.done","output_index":1,"item":message()}),
                completed(json!([malformed_message])),
            ])
            .into_bytes(),
            false,
        ),
        (
            sse([completed(json!([search_call(), bad_url]))]).into_bytes(),
            false,
        ),
        (
            sse([completed(json!("secret-invalid-output"))]).into_bytes(),
            false,
        ),
        (
            [success().into_bytes(), b"data: \xff\n\n".to_vec()].concat(),
            true,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(events)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let bus = Arc::new(EventBus::new(16));
        let mut receiver = bus.subscribe();
        let client = client(&server, store("token", "account", u64::MAX), Some(bus));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut request = request();
        let observed = calls.clone();
        request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
        let result = client.search(&ProviderAuth::new("unused"), &request).await;
        assert_eq!(result.is_ok(), succeeds, "{result:?}");
        assert_eq!(*calls.lock().unwrap(), [usage()]);
        let mut usage_count = 0;
        let mut terminals = 0;
        while let Some(event) = receiver.recv().now_or_never() {
            match event.unwrap().kind {
                EventKind::Usage(_) => usage_count += 1,
                EventKind::Provider(ProviderEvent::RequestCompleted { .. }) => terminals += 1,
                EventKind::Provider(ProviderEvent::RequestFailed { .. }) => {
                    panic!("completed usage owns the terminal observation")
                }
                _ => {}
            }
        }
        assert_eq!((usage_count, terminals), (1, 1));
    }
}

#[tokio::test]
async fn hosted_search_does_not_replace_the_chat_cache_baseline() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            let result = if body["tools"][0]["type"] == "web_search" {
                success()
            } else {
                sse([json!({"type":"response.completed", "response":{"usage":{"input_tokens":100,"output_tokens":1,"input_tokens_details":{"cached_tokens":80}}}})])
            };
            ResponseTemplate::new(200).set_body_raw(result, "text/event-stream")
        })
        .expect(3)
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let client = client(&server, store("token", "account", u64::MAX), Some(bus));
    let auth = ProviderAuth::new("unused");
    let search = request();
    let mut chat = chat_request();
    chat.observation = search.observation.clone();
    chat.observation.as_mut().unwrap().purpose = event_bus::RequestPurpose::Agent;
    client.send(&auth, &chat).await.unwrap();
    client.search(&auth, &search).await.unwrap();
    client.send(&auth, &chat).await.unwrap();
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
            previous_cache_tokens: 80,
            ..
        }
    ));
}
