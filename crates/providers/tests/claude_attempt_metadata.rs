use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use event_bus::RequestPurpose;
use futures_util::StreamExt;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::{
    ChatRequest, ObservationContext, ProviderAuth, ProviderClient, RequestTurnContext,
};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn input() -> ChatRequest {
    let mut request: ChatRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4-6", "messages":[
            {"role":"user","content":[{"type":"text","text":"Reply LOCAL_CAPTURE_OK."}]}
        ]
    }))
    .unwrap();
    request.observation = Some(ObservationContext {
        run_id: "session-one".into(),
        purpose: RequestPurpose::Agent,
        turn: Some(RequestTurnContext {
            prompt_id: "33333333-3333-4333-8333-333333333333".into(),
            origin: Some("human".into()),
            prompt_index: Some(1),
            turn_index: Some(1),
            is_subagent: false,
        }),
    });
    request
}

fn client(server: &MockServer) -> AnthropicClient {
    AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap()
    .with_claude_oauth()
    .unwrap()
}

fn json_response(request_id: &'static str, message_id: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).insert_header("request-id", request_id).set_body_json(json!({
        "id":message_id,"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],
        "stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":1}
    }))
}

fn sse(message_id: &str, terminal: &str) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"{message_id}\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{{\"input_tokens\":10,\"output_tokens\":0}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"ok\"}}}}\n\n{terminal}"
    )
}

#[tokio::test]
async fn successful_send_stream_send_chain_uses_provider_ids_without_changing_model_input() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(
            move |_: &wiremock::Request| match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => json_response("req_send", "msg_send"),
                1 => ResponseTemplate::new(200)
                    .insert_header("request-id", "req_stream")
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(
                        "msg_stream",
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                    )),
                _ => json_response("req_last", "msg_last"),
            },
        )
        .expect(3)
        .mount(&server)
        .await;
    let client = client(&server);
    let auth = ProviderAuth::new("dummy-token");
    let input = input();
    client.send(&auth, &input).await.unwrap();
    let events = client
        .stream(&auth, &input)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(Ok(providers::StreamEvent::Completed { .. }))
    ));
    client.send(&auth, &input).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let mut ids = std::collections::HashSet::new();
    for (index, request) in requests.iter().enumerate() {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let billing = body["system"][0]["text"].as_str().unwrap();
        assert_eq!(
            body["diagnostics"]["previous_message_id"],
            [Value::Null, json!("msg_send"), json!("msg_stream")][index]
        );
        match index {
            0 => assert!(!billing.contains("cc_prev_req=")),
            1 => assert!(billing.contains("cc_prev_req=req_send;")),
            _ => assert!(billing.contains("cc_prev_req=req_stream;")),
        }
        assert!(billing.contains("cc_prompt_id=33333333-3333-4333-8333-333333333333; cc_turn_origin=human; cc_prompt_index=1; cc_turn_index=1;"));
        assert!(!billing.contains("cc_is_subagent="));
        assert_eq!(
            request.headers["x-claude-code-prompt-id"],
            "33333333-3333-4333-8333-333333333333"
        );
        assert_eq!(request.headers["x-claude-code-request-class"], "main");
        let request_id = request.headers["x-client-request-id"].to_str().unwrap();
        assert!(uuid::Uuid::parse_str(request_id).is_ok());
        assert!(ids.insert(request_id));
        assert!(
            request.headers["anthropic-beta"]
                .to_str()
                .unwrap()
                .ends_with("cache-diagnosis-2026-04-07")
        );
        let model = mock_openai::claude_contract::claude_model_input(&request.body).unwrap();
        assert!(model.get("diagnostics").is_none());
        assert_eq!(
            model["messages"][0]["content"][0]["text"],
            "Reply LOCAL_CAPTURE_OK."
        );
    }
}

#[tokio::test]
async fn canceled_stream_and_rejected_or_invalid_responses_keep_previous_success() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(
            move |_: &wiremock::Request| match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => json_response("req_good", "msg_good"),
                1 => ResponseTemplate::new(200)
                    .insert_header("request-id", "req_canceled")
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse("msg_canceled", "")),
                2 => ResponseTemplate::new(429).insert_header("request-id", "req_rejected"),
                3 => ResponseTemplate::new(200)
                    .insert_header("request-id", "req_invalid_json")
                    .set_body_string("{"),
                4 => ResponseTemplate::new(200)
                    .insert_header("request-id", "req_invalid_stream")
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(
                        "msg_invalid_stream",
                        "event: message_stop\ndata: {\n\n",
                    )),
                _ => json_response("req_last", "msg_last"),
            },
        )
        .expect(6)
        .mount(&server)
        .await;
    let client = client(&server);
    let auth = ProviderAuth::new("dummy-token");
    let input = input();
    client.send(&auth, &input).await.unwrap();
    let mut canceled = client.stream(&auth, &input).await.unwrap();
    assert!(canceled.next().await.unwrap().is_ok());
    drop(canceled);
    assert!(client.send(&auth, &input).await.is_err());
    assert!(client.send(&auth, &input).await.is_err());
    let invalid = client
        .stream(&auth, &input)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(invalid.iter().any(Result::is_err));
    client.send(&auth, &input).await.unwrap();
    for request in server.received_requests().await.unwrap().iter().skip(1) {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["diagnostics"]["previous_message_id"], "msg_good");
        assert!(
            body["system"][0]["text"]
                .as_str()
                .unwrap()
                .contains("cc_prev_req=req_good;")
        );
    }
}
