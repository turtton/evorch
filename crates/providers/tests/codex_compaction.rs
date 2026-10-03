#[path = "support/codex.rs"]
mod codex_support;
#[path = "support/codex_contract.rs"]
mod contract_support;

use codex_support::{client, request};
use contract_support::{CodexBodyMatcher, CodexIdMatcher, seeded_store};
use providers::{Compactor, ContentBlock, ProviderClient, Usage};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SUCCESS: &str = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque+/=\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":30}}}}\n\n";

#[tokio::test]
async fn compact_uses_responses_auth_and_returns_opaque_state_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer access-tok-1"))
        .and(header("chatgpt-account-id", "acc-123"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("OpenAI-Beta", "responses=experimental"))
        .and(header("accept", "text/event-stream"))
        .and(CodexBodyMatcher)
        .and(CodexIdMatcher)
        .respond_with(ResponseTemplate::new(200).set_body_raw(SUCCESS, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, seeded_store());
    let mut request = request();
    request.messages[0].content.push(ContentBlock::Compaction {
        encrypted_content: "previous".into(),
    });
    let result = client.compactor().unwrap().compact(&request).await.unwrap();
    assert_eq!(result.encrypted_content, "opaque+/=");
    assert_eq!(
        result.usage,
        Usage {
            input_tokens: 42,
            output_tokens: 7,
            cache_read_tokens: 30,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        }
    );
    let received = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        body,
        serde_json::to_value(providers::wire::codex::to_wire_compaction_request(&request)).unwrap()
    );
    assert_eq!(
        body["input"][1],
        json!({"type":"compaction", "encrypted_content":"previous"})
    );
    assert_eq!(body["input"][2], json!({"type":"compaction_trigger"}));
}

#[tokio::test]
async fn compact_rejects_http_errors_missing_blob_and_premature_eof() {
    for (status, body) in [
        (503, "unavailable"),
        (
            200,
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        ),
        (
            200,
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\n",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            client(&server, seeded_store())
                .compact(&request())
                .await
                .is_err()
        );
    }
}
