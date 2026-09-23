#[path = "support/codex.rs"]
mod codex_support;
#[allow(dead_code)]
#[path = "support/codex_contract.rs"]
mod contract_support;
#[allow(dead_code)]
mod support;

use std::time::Duration;

use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{ChatRequest, JsonSchema, ProviderAuth, ProviderClient};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer};

fn output_schema() -> JsonSchema {
    JsonSchema {
        name: "escalation_verdict".into(),
        schema: json!({
            "type": "object",
            "properties": {"approve": {"type": "boolean"}},
            "required": ["approve"],
            "additionalProperties": false,
        }),
    }
}

#[test]
fn canonical_output_schema_round_trips_and_is_omitted_by_default() {
    let mut request: ChatRequest =
        serde_json::from_value(json!({"model": "test-model", "messages": []})).unwrap();
    assert!(request.output_schema.is_none());
    let baseline = serde_json::to_value(&request).unwrap();
    assert!(baseline.get("output_schema").is_none());
    request.output_schema = Some(output_schema());
    let serialized = serde_json::to_value(&request).unwrap();
    let restored: ChatRequest = serde_json::from_value(serialized.clone()).unwrap();
    assert_eq!(restored, request);
    assert_eq!(
        serialized["output_schema"]["schema"],
        output_schema().schema
    );
}

#[tokio::test]
async fn openai_and_compatible_send_strict_schema_only_when_configured() {
    for compatible in [false, true] {
        let server = MockServer::start().await;
        for streaming in [false, true] {
            let response = if streaming {
                support::sse_response(&support::fixture("openai", "stream_text.sse"))
            } else {
                support::json_response(200, &support::fixture("openai", "send_text.json"))
            };
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .and(body_partial_json(json!({"stream": streaming})))
                .respond_with(response)
                .expect(2)
                .mount(&server)
                .await;
        }
        let client: Box<dyn ProviderClient> = if compatible {
            Box::new(
                OpenAiCompatibleClient::new(server.uri(), "test", Duration::from_secs(5), None)
                    .unwrap(),
            )
        } else {
            Box::new(
                OpenAiClient::new(OpenAiConfig {
                    base_url: server.uri(),
                    timeout: Duration::from_secs(5),
                    event_bus: None,
                })
                .unwrap(),
            )
        };
        assert!(client.supports_structured_output());
        for streaming in [false, true] {
            send_with_and_without_schema(client.as_ref(), streaming).await;
        }
        let bodies = request_bodies(&server).await;
        for pair in bodies.chunks_exact(2) {
            assert_eq!(
                pair[1]["response_format"],
                json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "escalation_verdict", "strict": true,
                        "schema": output_schema().schema,
                    },
                })
            );
            assert_only_format_changed(&pair[0], &pair[1], "response_format");
        }
    }
}

#[tokio::test]
async fn codex_send_and_stream_send_text_format_only_when_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(support::sse_response(&support::fixture(
            "codex",
            "responses_success.sse",
        )))
        .expect(4)
        .mount(&server)
        .await;
    let client = codex_support::client(&server, contract_support::seeded_store());
    assert!(client.supports_structured_output());
    for streaming in [false, true] {
        send_with_and_without_schema(&client, streaming).await;
    }
    let bodies = request_bodies(&server).await;
    for pair in bodies.chunks_exact(2) {
        assert_eq!(
            pair[1]["text"],
            json!({"format": {
                "type": "json_schema", "name": "escalation_verdict", "strict": true,
                "schema": output_schema().schema,
            }})
        );
        assert_only_format_changed(&pair[0], &pair[1], "text");
    }
}

async fn send_with_and_without_schema(client: &dyn ProviderClient, streaming: bool) {
    for schema in [None, Some(output_schema())] {
        let mut request = codex_support::request();
        request.output_schema = schema;
        if streaming {
            client
                .send_streaming(
                    &ProviderAuth::new("test-key"),
                    &request,
                    &event_bus::EventBus::new(16),
                )
                .await
                .unwrap();
        } else {
            client
                .send(&ProviderAuth::new("test-key"), &request)
                .await
                .unwrap();
        }
    }
}

async fn request_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

fn assert_only_format_changed(baseline: &Value, structured: &Value, field: &str) {
    assert!(baseline.get(field).is_none());
    let mut without_format = structured.clone();
    without_format.as_object_mut().unwrap().remove(field);
    assert_eq!(
        baseline, &without_format,
        "all ordinary request fields remain unchanged"
    );
}
