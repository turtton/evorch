use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::{
    ChatRequest, ObservationContext, ProviderAuth, ProviderClient, RequestTurnContext,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn api_key_messages_wire_is_unchanged_by_runtime_turn_provenance() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}))).expect(2).mount(&server).await;
    let client = AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap();
    let mut request: ChatRequest = serde_json::from_value(json!({"model":"claude-test","messages":[{"role":"user","content":[{"type":"text","text":"stable"}]}]})).unwrap();
    request.observation = Some(ObservationContext {
        run_id: "run".into(),
        ..Default::default()
    });
    let canonical = serde_json::to_vec(&request).unwrap();
    client
        .send(&ProviderAuth::new("api-key"), &request)
        .await
        .unwrap();
    request.observation.as_mut().unwrap().turn = Some(RequestTurnContext {
        origin: Some("human".into()),
        prompt_index: Some(1),
        turn_index: Some(1),
        ..RequestTurnContext::unattributed(false)
    });
    assert_eq!(serde_json::to_vec(&request).unwrap(), canonical);
    client
        .send(&ProviderAuth::new("api-key"), &request)
        .await
        .unwrap();
    let sent = server.received_requests().await.unwrap();
    assert_eq!(sent[0].body, sent[1].body);
    let body: serde_json::Value = sent[1].body_json().unwrap();
    assert!(body.get("metadata").is_none());
    assert!(body.get("observation").is_none());
}
