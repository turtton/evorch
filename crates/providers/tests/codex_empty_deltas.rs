use event_bus::{EventBus, EventKind, MessageEvent, ProviderEvent};
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{ChatRequest, ContentBlock, ProviderAuth, ProviderClient};
use std::sync::Arc;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn empty_deltas_are_noops_and_reasoning_alone_has_no_ttft() {
    // Given: empty text/reasoning around a reasoning-only response.
    let server = MockServer::start().await;
    let body = [
        r#"{"type":"response.output_text.delta","delta":""}"#,
        r#"{"type":"response.reasoning_summary_text.delta","delta":""}"#,
        r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
        r#"{"type":"response.output_text.delta","delta":""}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":2}}}"#,
    ].map(|data| format!("data: {data}\n\n")).concat();
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
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let client = CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            event_bus: Some(bus.clone()),
            ..CodexConfig::default()
        },
        store,
    )
    .expect("client");
    let request = ChatRequest {
        model: "codex".into(),
        messages: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        observation: None,
    };
    // When: complete over real HTTP and terminate event collection with a sentinel.
    let response = client
        .send_streaming(&ProviderAuth::new(""), &request, &bus)
        .await
        .expect("completion");
    bus.emit(event_bus::Event::new(MessageEvent::MessageDelta {
        delta: "sentinel".into(),
        run_id: None,
    }));
    let mut deltas = Vec::new();
    let mut first_tokens = 0;
    loop {
        match receiver.recv().await.expect("bus").kind {
            EventKind::Message(MessageEvent::MessageDelta { delta, .. }) if delta == "sentinel" => {
                break;
            }
            EventKind::Message(
                MessageEvent::MessageDelta { delta, .. }
                | MessageEvent::ReasoningDelta { delta, .. },
            ) => deltas.push(delta),
            EventKind::Provider(ProviderEvent::FirstTokenObserved { .. }) => first_tokens += 1,
            _ => {}
        }
    }
    // Then: only nonempty reasoning exists, and the existing text/tool-only TTFT contract holds.
    assert_eq!(deltas, ["thinking"]);
    assert_eq!(first_tokens, 0);
    assert_eq!(
        response.message.content,
        [ContentBlock::Reasoning {
            text: "thinking".into()
        }]
    );
}
