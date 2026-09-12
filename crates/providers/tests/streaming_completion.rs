use event_bus::{EventBus, EventKind, MessageEvent, ProviderEvent};
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{ContentBlock, ObservationContext, ProviderAuth, ProviderClient};
use std::sync::Arc;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> providers::ChatRequest {
    providers::ChatRequest {
        model: "gpt-5.1-codex".into(),
        messages: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        observation: None,
    }
}

#[tokio::test]
async fn codex_streaming_preserves_reasoning_text_and_first_token() {
    // Given: real Codex parsing with reasoning followed by text and completion.
    let server = MockServer::start().await;
    let body = format!(
        "event: response.reasoning_summary_text.delta\ndata: {{\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking\"}}\n\n{}",
        include_str!("fixtures/codex/responses_success.sse")
    );
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let store = Arc::new(InMemoryTokenStore::new());
    store.save(&TokenBundle {
        access_token: "access".into(), refresh_token: "refresh".into(),
        id_token: "e30.eyJleHAiOjQxMDI0NDQ4MDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2MifX0.signature".into(),
    }).expect("tokens");
    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let client = CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            event_bus: Some(Arc::clone(&bus)),
            ..CodexConfig::default()
        },
        store,
    )
    .expect("client");
    let mut request = request();
    request.observation = Some(ObservationContext {
        run_id: "codex-run".into(),
    });
    // When: consume through the new completion path rather than raw DeltaStream.
    let response = client
        .send_streaming(&ProviderAuth::new(""), &request, &bus)
        .await
        .expect("streaming completion");
    // Then: canonical content and attributed live event fragments agree.
    assert_eq!(
        response.message.content,
        vec![
            ContentBlock::Reasoning {
                text: "thinking".into()
            },
            ContentBlock::Text {
                text: "Hello world".into()
            },
        ]
    );
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut first_tokens = 0;
    while text != "Hello world" || reasoning != "thinking" || first_tokens == 0 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
            .await
            .expect("events timeout")
            .expect("bus");
        match event.kind {
            EventKind::Message(MessageEvent::MessageDelta { delta, run_id }) => {
                assert_eq!(run_id.as_deref(), Some("codex-run"));
                text.push_str(&delta);
            }
            EventKind::Message(MessageEvent::ReasoningDelta { delta, run_id }) => {
                assert_eq!(run_id.as_deref(), Some("codex-run"));
                reasoning.push_str(&delta);
            }
            EventKind::Provider(ProviderEvent::FirstTokenObserved { .. }) => first_tokens += 1,
            _ => {}
        }
    }
    assert_eq!(
        (text.as_str(), reasoning.as_str(), first_tokens),
        ("Hello world", "thinking", 1)
    );
}

#[tokio::test]
async fn streaming_completion_rejects_premature_eof() {
    // Given: an OpenAI server that closes after a valid delta without DONE.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
                ),
        )
        .mount(&server)
        .await;
    let client =
        providers::provider::openai::OpenAiClient::new(providers::provider::openai::OpenAiConfig {
            base_url: server.uri(),
            ..providers::provider::openai::OpenAiConfig::default()
        })
        .expect("client");
    let bus = EventBus::new(8);
    // When: asking for a canonical completion from an incomplete stream.
    let result = client
        .send_streaming(&ProviderAuth::new("key"), &request(), &bus)
        .await;
    // Then: partial content is not silently accepted as success.
    assert!(matches!(result, Err(providers::ProviderError::Request(_))));
}
