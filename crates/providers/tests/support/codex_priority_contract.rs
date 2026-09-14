use super::*;
use wiremock::matchers::body_partial_json;

#[tokio::test]
async fn send_priority_tier_in_codex_body() {
    // Given: priorityを要求するcanonical JSONと厳密な本文契約。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(body_partial_json(json!({"service_tier": "priority"})))
        .respond_with(sse_response(&fixture("codex", "responses_success.sse")))
        .expect(1)
        .mount(&server)
        .await;
    let mut input = serde_json::to_value(request()).expect("request JSON");
    input["service_tier"] = json!("priority");
    let request = serde_json::from_value(input).expect("priority request");
    // When: send / Then: priority本文に一致して完了する。
    client(&server, seeded_store())
        .send(&ProviderAuth::new(""), &request)
        .await
        .expect("priority send succeeds");
}
