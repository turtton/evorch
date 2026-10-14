use super::*;

const OVERLOAD_MESSAGE: &str = "Our servers are currently overloaded. Please try again later.";

#[tokio::test]
async fn streaming_recovers_from_http_and_sse_overload_failures() {
    for response in [
        json_response(400, &format!("server_is_overloaded: {OVERLOAD_MESSAGE}")),
        ResponseTemplate::new(400).set_body_json(json!({
            "error": {"code": "server_is_overloaded", "message": OVERLOAD_MESSAGE},
        })),
        sse_response(&format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "server_is_overloaded", "message": OVERLOAD_MESSAGE}},
            })
        )),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(response)
            .up_to_n_times(2)
            .with_priority(1)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(sse_response(&fixture("codex", "responses_success.sse")))
            .with_priority(2)
            .expect(1)
            .mount(&server)
            .await;

        let response = client(&server, seeded_store())
            .send_streaming(&ProviderAuth::new(""), &request(), &EventBus::new(64))
            .await
            .expect("third attempt recovers from overload");
        assert_eq!(response.finish_reason, FinishReason::Stop);
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| {
            request.body == requests[0].body
                && request.headers["session-id"] == requests[0].headers["session-id"]
                && CodexBodyMatcher.matches(request)
                && CodexIdMatcher.matches(request)
        }));
        server.verify().await;
    }
}

#[tokio::test]
async fn streaming_stops_after_three_overloaded_sse_responses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(sse_response(&format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "server_is_overloaded", "message": OVERLOAD_MESSAGE}},
            })
        )))
        .expect(3)
        .mount(&server)
        .await;

    let error = client(&server, seeded_store())
        .send_streaming(&ProviderAuth::new(""), &request(), &EventBus::new(64))
        .await
        .expect_err("persistent overload exhausts retries");
    assert_eq!(
        error,
        ProviderError::RetriesExhausted {
            attempts: 3,
            last: Box::new(ProviderError::Http {
                status: 400,
                body: format!("server_is_overloaded: {OVERLOAD_MESSAGE}"),
            }),
        }
    );
}

#[tokio::test]
async fn streaming_does_not_retry_unrelated_sse_rejections() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse_response(&fixture("codex", "responses_failed.sse")),
    )
    .await;

    let error = client(&server, seeded_store())
        .send_streaming(&ProviderAuth::new(""), &request(), &EventBus::new(64))
        .await
        .expect_err("invalid requests fail immediately");
    assert_eq!(
        error,
        ProviderError::Http {
            status: 400,
            body: "invalid_request_error: request rejected".into(),
        }
    );
}
