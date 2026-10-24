#[allow(dead_code)]
mod support;

use std::time::Duration;

use futures_util::StreamExt;
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{ChatRequest, ProviderAuth, ProviderClient};
use serde_json::{Value, json};
use support::{fixture, json_response, sse_response};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

#[tokio::test]
async fn clients_choose_the_output_limit_field_for_send_and_stream() {
    for streaming in [false, true] {
        let server = MockServer::start().await;
        // The official provider always answers over Responses SSE.
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(sse_response(&fixture("openai", "responses_text.sse")))
            .mount(&server)
            .await;
        let compatible_response = if streaming {
            sse_response(&fixture("openai", "stream_text.sse"))
        } else {
            json_response(200, &fixture("openai", "send_text.json"))
        };
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(compatible_response)
            .mount(&server)
            .await;
        let mut expected = Vec::new();
        for provider in ["openai", "compatible", "kimi"] {
            let client: Box<dyn ProviderClient> = if provider == "openai" {
                Box::new(
                    OpenAiClient::new(OpenAiConfig {
                        base_url: server.uri(),
                        ..Default::default()
                    })
                    .unwrap(),
                )
            } else {
                Box::new(
                    OpenAiCompatibleClient::new(
                        server.uri(),
                        provider,
                        Duration::from_secs(60),
                        None,
                    )
                    .unwrap(),
                )
            };
            // The client determines the parameter, including for identical model IDs.
            for model in ["o1", "gpt-4o", "kimi-k2.5"] {
                for limit in [Some(128_000_u64), None] {
                    let request: ChatRequest = serde_json::from_value(json!({
                        "model": model,
                        "max_tokens": limit,
                        "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}]
                    })).unwrap();
                    let auth = ProviderAuth::new("test-key");
                    if streaming {
                        let mut stream = client.stream(&auth, &request).await.unwrap();
                        while let Some(event) = stream.next().await {
                            event.unwrap();
                        }
                    } else {
                        client.send(&auth, &request).await.unwrap();
                    }
                    expected.push((provider, model, limit));
                }
            }
        }
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), expected.len());
        for (request, (provider, model, limit)) in received.iter().zip(expected) {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["model"], model);
            let (endpoint, field, transport_streams) = if provider == "openai" {
                ("/responses", "max_output_tokens", true)
            } else {
                ("/chat/completions", "max_tokens", streaming)
            };
            assert_eq!(request.url.path(), endpoint, "{provider}");
            assert_eq!(body["stream"], transport_streams, "{provider}: {body}");
            for absent in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                if absent != field {
                    assert!(body.get(absent).is_none(), "{provider}: {body}");
                }
            }
            assert_eq!(
                body.get(field).and_then(Value::as_u64),
                limit,
                "{provider}: {body}"
            );
            if limit.is_none() {
                assert!(body.get(field).is_none(), "{provider}: {body}");
            }
        }
    }
}
