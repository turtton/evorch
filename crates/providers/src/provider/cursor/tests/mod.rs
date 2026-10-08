mod arguments;
mod auth;
mod cache;
mod models;
mod models_admin;
mod quota;
mod streaming;

use super::*;
use crate::{
    ContentBlock, FinishReason, Message, ObservationContext, Role, ToolResultContent, ToolSpec,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

fn token() -> CursorTokenBundle {
    CursorTokenBundle {
        access_token: format!(
            "a.{}.z",
            URL_SAFE_NO_PAD.encode(br#"{"exp":9999999999,"sub":"auth0|user_1"}"#)
        ),
        refresh_token: "refresh-secret".into(),
        expires_at: 9_999_999_999,
        account_id: Some("user_1".into()),
        email: Some("user@example.test".into()),
    }
}
fn oauth(server: &MockServer) -> CursorOAuthClient {
    CursorOAuthClient::new(CursorOAuthConfig {
        login_url: format!("{}/login", server.uri()),
        poll_url: format!("{}/poll", server.uri()),
        token_url: format!("{}/token", server.uri()),
        profile_url: format!("{}/profile", server.uri()),
    })
    .unwrap()
}
fn request() -> ChatRequest {
    ChatRequest {
        model: "model-1".into(),
        messages: vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "System instructions".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Use lookup".into(),
                }],
            },
        ],
        tools: vec![ToolSpec {
            name: "lookup".into(),
            description: "Lookup data".into(),
            input_schema: json!({"type":"object", "properties":{"query":{"type":"string"}}, "required":["query"]}),
        }],
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: Some(ObservationContext {
            run_id: "cursor-contract".into(),
            ..Default::default()
        }),
    }
}
fn client(server: &MockServer) -> CursorClient {
    let store = Arc::new(InMemoryCursorTokenStore::new());
    store.save(&token()).unwrap();
    CursorClient::new(
        CursorConfig {
            base_url: server.uri(),
            event_bus: None,
        },
        store,
    )
    .unwrap()
}
fn response(messages: Vec<Proto>) -> ResponseTemplate {
    let mut bytes: Vec<_> = messages
        .into_iter()
        .flat_map(|m| wire::frame(&m.0))
        .collect();
    bytes.extend_from_slice(&[2, 0, 0, 0, 2, b'{', b'}']);
    ResponseTemplate::new(200).set_body_raw(bytes, "application/connect+proto")
}
fn end() -> Proto {
    Proto::new().message(
        1,
        Proto::new().message(
            14,
            Proto::new()
                .integer(1, 100)
                .integer(2, 20)
                .integer(3, 75)
                .integer(4, 5),
        ),
    )
}
fn child(bytes: &[u8], number: u32) -> &[u8] {
    wire::nested(bytes, number).unwrap().unwrap()
}

#[tokio::test]
async fn discovery_uses_account_models_and_falls_back_to_rich_catalog() {
    let server = MockServer::start().await;
    Mock::given(path("/agent.v1.AgentService/GetUsableModels"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                Proto::new()
                    .message(1, Proto::new().string(1, "model-max").integer(7, 1))
                    .0,
                "application/proto",
            ),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/agent.v1.AgentService/GetUsableModels"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(path("/aiserver.v1.AiService/AvailableModels"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                Proto::new()
                    .message(
                        2,
                        Proto::new()
                            .string(1, "picker-label")
                            .string(18, "wire-model")
                            .integer(5, 1),
                    )
                    .0,
                "application/proto",
            ),
        )
        .mount(&server)
        .await;
    let client = client(&server);
    assert_eq!(
        client
            .list_models(&ProviderAuth::new(""))
            .await
            .unwrap()
            .unwrap(),
        ["model-max"]
    );
    assert_eq!(
        client
            .list_models(&ProviderAuth::new(""))
            .await
            .unwrap()
            .unwrap(),
        ["picker-label"]
    );
}

#[tokio::test]
async fn empty_usable_catalog_falls_back_to_valid_agent_models() {
    let server = MockServer::start().await;
    Mock::given(path("/agent.v1.AgentService/GetUsableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(Vec::new(), "application/proto"))
        .mount(&server)
        .await;
    let available = Proto::new()
        .message(2, Proto::new().string(1, "default").integer(5, 1))
        .message(2, Proto::new().string(1, "chat-only").integer(4, 1))
        .message(2, Proto::new().string(1, "needs-retention").integer(46, 1));
    Mock::given(path("/aiserver.v1.AiService/AvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(available.0, "application/proto"))
        .mount(&server)
        .await;
    assert_eq!(
        client(&server)
            .list_models(&ProviderAuth::new(""))
            .await
            .unwrap()
            .unwrap(),
        ["default"]
    );
}

#[tokio::test]
async fn dropping_stream_releases_conversation_and_emits_no_completed_usage() {
    let server = MockServer::start().await;
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![end()]))
        .mount(&server)
        .await;
    let store = Arc::new(InMemoryCursorTokenStore::new());
    store.save(&token()).unwrap();
    let bus = Arc::new(event_bus::EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = CursorClient::new(
        CursorConfig {
            base_url: server.uri(),
            event_bus: Some(bus),
        },
        store,
    )
    .unwrap();
    let stream = client
        .stream(&ProviderAuth::new(""), &request())
        .await
        .unwrap();
    let session = client
        .sessions
        .lock()
        .await
        .values()
        .next()
        .unwrap()
        .clone();
    assert!(session.try_lock().is_err());
    drop(stream);
    assert!(session.try_lock().is_ok());
    use futures_util::FutureExt;
    let mut completed = 0;
    while let Some(Ok(event)) = receiver.recv().now_or_never() {
        if matches!(event.kind, event_bus::EventKind::Usage(_)) {
            completed += 1;
        }
    }
    assert_eq!(completed, 0);
}

#[tokio::test]
async fn connect_error_and_truncated_stream_never_report_success() {
    let server = MockServer::start().await;
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = client(&server);
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![Proto::new().message(
            1,
            Proto::new().message(1, Proto::new().string(1, "partial")),
        )]))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    assert!(matches!(
        client.send(&ProviderAuth::new(""), &request()).await,
        Err(ProviderError::InvalidSse { .. })
    ));
    let payload = br#"{"error":{"code":"unauthenticated","message":"access-secret"}}"#;
    let mut bytes = wire::frame(payload);
    bytes[0] = 2;
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(bytes, "application/connect+proto"))
        .mount(&server)
        .await;
    let error = client
        .send(&ProviderAuth::new(""), &request())
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(401));
    assert!(!error.to_string().contains("access-secret"));
}

#[test]
fn protobuf_values_and_invalid_lengths_are_checked_against_wire_literals() {
    assert_eq!(wire::encode_value(&json!("x")).0, [0x1a, 1, b'x']);
    assert_eq!(wire::encode_value(&json!(true)).0, [0x20, 1]);
    assert!(wire::decode_value(&[0x32, 7, 0x0a, 2, 0x20, 1, 0x0a, 1, 0x00]).is_err());
    for value in [
        json!(null),
        json!({"array":[1,true,"x",null],"object":{"a":2}}),
    ] {
        assert_eq!(
            wire::decode_value(&wire::encode_value(&value).0).unwrap(),
            value
        );
    }
    assert!(wire::fields(&[0x0a, 0xff, 0xff, 0xff, 0xff, 0x0f]).is_err());
    assert!(wire::fields(&[0]).is_err());
    assert!(
        wire::fields(&[
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02
        ])
        .is_err()
    );
}
