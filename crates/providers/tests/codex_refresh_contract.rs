use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use event_bus::EventBus;
use providers::provider::codex::oauth::CODEX_CLIENT_ID;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{
    ChatRequest, ContentBlock, Message, ProviderAuth, ProviderClient, ProviderError, Role,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SUCCESS_SSE: &str = include_str!("fixtures/codex/responses_success.sse");

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5.1-codex".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(123),
        observation: None,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after epoch")
        .as_secs()
}

fn make_dummy_jwt(exp: u64, account_id: &str) -> String {
    let payload = json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

fn seeded_store(expires_in: u64) -> Arc<InMemoryTokenStore> {
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(&TokenBundle {
            access_token: "access-tok-1".to_string(),
            refresh_token: "refresh-old".to_string(),
            id_token: make_dummy_jwt(now_unix() + expires_in, "acc-123"),
        })
        .expect("token bundle can be seeded");
    store
}

fn client(
    server: &MockServer,
    store: Arc<dyn CodexTokenStore>,
    event_bus: Option<Arc<EventBus>>,
) -> CodexClient {
    CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            timeout: Duration::from_secs(2),
            event_bus,
        },
        store,
    )
    .expect("Codex client can be built")
}

async fn mount_refresh(server: &MockServer, response: ResponseTemplate, expected: u64) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(json!({
            "client_id": CODEX_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": "refresh-old"
        })))
        .respond_with(response)
        .expect(expected)
        .mount(server)
        .await;
}

async fn mount_responses(server: &MockServer, access_token: &str, expected: u64) {
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {access_token}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SUCCESS_SSE, "text/event-stream"))
        .expect(expected)
        .mount(server)
        .await;
}

fn refreshed_token_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": "access-tok-2",
        "refresh_token": "refresh-new",
        "id_token": make_dummy_jwt(now_unix() + 7_200, "acc-123")
    }))
}

// Given: 更新猶予時間内の bundle / When: send / Then: refresh 済み token で送信し bundle を保存する
#[tokio::test]
async fn send_auto_refreshes_expiring_token() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_token_response(), 1).await;
    mount_responses(&server, "access-tok-2", 1).await;
    let store = seeded_store(60);

    let result = client(&server, store.clone(), None)
        .send(&ProviderAuth::new(""), &request())
        .await;

    assert!(result.is_ok());
    let persisted = store
        .load()
        .expect("persisted bundle can be loaded")
        .expect("refreshed bundle is present");
    assert_eq!(persisted.access_token, "access-tok-2");
    assert_eq!(persisted.refresh_token, "refresh-new");
}

// Given: 更新猶予時間外の bundle / When: send / Then: refresh せず元の token で送信する
#[tokio::test]
async fn send_skips_refresh_when_token_fresh() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_token_response(), 0).await;
    mount_responses(&server, "access-tok-1", 1).await;

    let result = client(&server, seeded_store(3_600), None)
        .send(&ProviderAuth::new(""), &request())
        .await;

    assert!(result.is_ok());
}

// Given: 同じ期限間近 bundle を使う2送信 / When: 同時に send / Then: refresh は1回だけで両方成功する
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_sends_refresh_once() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_token_response(), 1).await;
    mount_responses(&server, "access-tok-2", 2).await;
    let client = client(&server, seeded_store(60), None);
    let auth = ProviderAuth::new("");
    let first_request = request();
    let second_request = request();

    let (first, second) = tokio::join!(
        client.send(&auth, &first_request),
        client.send(&auth, &second_request)
    );

    assert!(first.is_ok());
    assert!(second.is_ok());
}

// Given: refresh が401になる bundle と EventBus / When: send / Then: エラーを返し backend・Started・Usage は発生しない
#[tokio::test]
async fn refresh_failure_surfaces_error_and_zero_usage() {
    let server = MockServer::start().await;
    mount_refresh(
        &server,
        ResponseTemplate::new(401).set_body_string("expired refresh token"),
        1,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(8));
    let mut events = bus.subscribe();

    let error = client(&server, seeded_store(60), Some(bus))
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect_err("refresh failure surfaces from send");

    assert!(matches!(error, ProviderError::Http { status: 401, .. }));
    let event = tokio::time::timeout(Duration::from_millis(100), events.recv()).await;
    assert!(
        event.is_err(),
        "refresh failure must emit neither Started nor Usage: {event:?}"
    );
}
