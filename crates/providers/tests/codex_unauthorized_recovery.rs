//! Codex が 401 で拒否した認証情報を 1 回だけ回復して再送する契約。
//!
//! 再送は同一 wire body で行う (prompt cache の prefix を変えない)。

use std::io::Cursor;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use event_bus::{EventBus, EventKind, ProviderEvent, ProviderFailureKind};
use providers::provider::codex::oauth::CODEX_CLIENT_ID;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{
    ChatRequest, ContentBlock, HostedWebSearchError, ImageGenerationRequest, Message, ProviderAuth,
    ProviderClient, ProviderError, Role,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[allow(dead_code)]
#[path = "codex_web_search/support.rs"]
mod web_search_support;

const CHAT_SSE: &str = include_str!("fixtures/codex/responses_success.sse");
const COMPACTION_SSE: &str = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}}\n\n";
const RESPONSES_PATH: &str = "/backend-api/codex/responses";

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after epoch")
        .as_secs()
}

fn jwt(exp: u64, account: &str) -> String {
    let payload = json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account}
    });
    format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(payload.to_string()))
}

fn bundle(access: &str, refresh: &str, account: &str) -> TokenBundle {
    TokenBundle {
        access_token: access.into(),
        refresh_token: refresh.into(),
        id_token: jwt(now_unix() + 3_600, account),
    }
}

/// access token は不透明で、ID token は十分に有効。事前 refresh は起きない。
fn seeded_store() -> Arc<InMemoryTokenStore> {
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(&bundle("access-old", "refresh-old", "acc-123"))
        .expect("seed store");
    store
}

fn client(
    server: &MockServer,
    store: Arc<InMemoryTokenStore>,
    bus: Option<Arc<EventBus>>,
) -> CodexClient {
    CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            event_bus: bus,
            ..CodexConfig::default()
        },
        store,
    )
    .expect("Codex client can be built")
}

fn request() -> ChatRequest {
    ChatRequest {
        responses_replay: None,
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
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}

fn refresh_request_body() -> Value {
    json!({
        "client_id": CODEX_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": "refresh-old",
        "refresh_reason": "unauthorized_recovery"
    })
}

fn refreshed_response(account: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": "access-new",
        "refresh_token": "refresh-new",
        "id_token": jwt(now_unix() + 7_200, account)
    }))
}

async fn mount_refresh(server: &MockServer, response: impl Respond + 'static, expected: u64) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request_body()))
        .respond_with(response)
        .expect(expected)
        .mount(server)
        .await;
}

async fn mount_status(
    server: &MockServer,
    endpoint: &str,
    access: &str,
    response: ResponseTemplate,
) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .and(header("authorization", format!("Bearer {access}")))
        .respond_with(response)
        .mount(server)
        .await;
}

fn unauthorized() -> ResponseTemplate {
    ResponseTemplate::new(401).set_body_string("token expired")
}

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_owned(), "text/event-stream")
}

async fn requests_to(server: &MockServer, endpoint: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .expect("request recording is enabled")
        .into_iter()
        .filter(|request| request.url.path() == endpoint)
        .collect()
}

fn bearer(request: &Request) -> &str {
    request.headers["authorization"]
        .to_str()
        .expect("authorization is ascii")
}

fn assert_resent_with_new_token(sent: &[Request]) {
    assert_eq!(sent.len(), 2, "exactly one resend is allowed");
    assert_eq!(bearer(&sent[0]), "Bearer access-old");
    assert_eq!(bearer(&sent[1]), "Bearer access-new");
    assert_eq!(
        sent[0].body, sent[1].body,
        "the resend must reuse the identical wire body"
    );
}

// Given: access token を 401 で拒否される / When: send / Then: unauthorized_recovery で 1 回 refresh し、
// 同一 body を新しい Bearer で再送して成功し、bundle を保存する
#[tokio::test]
async fn chat_recovers_from_unauthorized_with_identical_body() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(&server, RESPONSES_PATH, "access-new", sse(CHAT_SSE)).await;
    let store = seeded_store();

    let result = client(&server, store.clone(), None)
        .send(&ProviderAuth::new(""), &request())
        .await;

    assert!(result.is_ok(), "{result:?}");
    assert_resent_with_new_token(&requests_to(&server, RESPONSES_PATH).await);
    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.access_token, "access-new");
    assert_eq!(saved.refresh_token, "refresh-new");
}

// Given: 同じ古い token を使う 2 リクエスト / When: 同時に 401 を受ける / Then: refresh は 1 回で両方成功する
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_unauthorized_responses_refresh_once() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(&server, RESPONSES_PATH, "access-new", sse(CHAT_SSE)).await;
    let client = client(&server, seeded_store(), None);
    let auth = ProviderAuth::new("");
    let (first_request, second_request) = (request(), request());

    let (first, second) = tokio::join!(
        client.send(&auth, &first_request),
        client.send(&auth, &second_request)
    );

    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_ok(), "{second:?}");
}

// Given: store が先に rotate 済み / When: 古い token が 401 / Then: refresh せず store の token で再送する
#[tokio::test]
async fn rotated_store_is_adopted_without_refresh() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 0).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(&server, RESPONSES_PATH, "access-new", sse(CHAT_SSE)).await;
    let store = seeded_store();
    let client = client(&server, store.clone(), None);
    let auth = ProviderAuth::new("");
    let request = request();
    client
        .resolve_responses_scope(&auth, &request)
        .await
        .expect("session caches the old bundle");
    store
        .save(&bundle("access-new", "refresh-new", "acc-123"))
        .unwrap();

    let result = client.send(&auth, &request).await;

    assert!(result.is_ok(), "{result:?}");
    assert_resent_with_new_token(&requests_to(&server, RESPONSES_PATH).await);
}

// Given: refresh が invalid_grant で store も不変 / When: send / Then: 再送せず再認証を要求する
#[tokio::test]
async fn invalid_grant_without_rotated_store_stops_for_reauthentication() {
    let server = MockServer::start().await;
    mount_refresh(
        &server,
        ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})),
        1,
    )
    .await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;

    let error = client(&server, seeded_store(), None)
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect_err("re-authentication is required");

    assert!(
        matches!(&error, ProviderError::Http { status: 401, body } if body.contains("re-authenticate")),
        "{error:?}"
    );
    assert_eq!(requests_to(&server, RESPONSES_PATH).await.len(), 1);
}

/// refresh 応答の返却前に、別プロセスが store を rotate した状況を再現する。
struct RotateStoreThenReject(Arc<InMemoryTokenStore>);

impl Respond for RotateStoreThenReject {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0
            .save(&bundle("access-new", "refresh-rotated", "acc-123"))
            .unwrap();
        ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"}))
    }
}

// Given: refresh が invalid_grant だが store は別経路で更新済み / When: send / Then: その token で再送して成功する
#[tokio::test]
async fn invalid_grant_adopts_store_rotated_during_refresh() {
    let server = MockServer::start().await;
    let store = seeded_store();
    mount_refresh(&server, RotateStoreThenReject(store.clone()), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(&server, RESPONSES_PATH, "access-new", sse(CHAT_SSE)).await;

    let result = client(&server, store, None)
        .send(&ProviderAuth::new(""), &request())
        .await;

    assert!(result.is_ok(), "{result:?}");
    assert_resent_with_new_token(&requests_to(&server, RESPONSES_PATH).await);
}

// Given: 回復後の token も 401 / When: send / Then: 2 回目の 401 をそのまま返し、これ以上再送しない
#[tokio::test]
async fn second_unauthorized_is_returned_without_further_retry() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(&server, RESPONSES_PATH, "access-new", unauthorized()).await;

    let error = client(&server, seeded_store(), None)
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect_err("second 401 is final");

    assert!(
        matches!(error, ProviderError::Http { status: 401, .. }),
        "{error:?}"
    );
    assert_eq!(requests_to(&server, RESPONSES_PATH).await.len(), 2);
}

// Given: 回復で ChatGPT account が変わる / When: send / Then: 別 account へ再送せず ResponsesScopeChanged
#[tokio::test]
async fn recovered_account_change_is_reported_as_scope_change() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-other"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;

    let error = client(&server, seeded_store(), None)
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect_err("account change must not be replayed");

    assert!(
        matches!(error, ProviderError::ResponsesScopeChanged { .. }),
        "{error:?}"
    );
    assert_eq!(requests_to(&server, RESPONSES_PATH).await.len(), 1);
}

// Given: compaction が 401 / When: compact / Then: 回復して同一 body で再送し結果を返す
#[tokio::test]
async fn compaction_recovers_from_unauthorized() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(&server, RESPONSES_PATH, "access-new", sse(COMPACTION_SSE)).await;
    let client = client(&server, seeded_store(), None);

    let result = client
        .compactor()
        .expect("codex compacts")
        .compact(&request())
        .await
        .expect("compaction recovers");

    assert_eq!(result.encrypted_content, "opaque");
    assert_resent_with_new_token(&requests_to(&server, RESPONSES_PATH).await);
}

// Given: hosted search が 401 / When: search / Then: 回復して同一 body で再送し成功する
#[tokio::test]
async fn hosted_search_recovers_from_unauthorized() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(
        &server,
        RESPONSES_PATH,
        "access-new",
        sse(&web_search_support::success()),
    )
    .await;
    let client = client(&server, seeded_store(), None);

    let result = client
        .hosted_web_search()
        .expect("codex searches")
        .search(&ProviderAuth::new(""), &web_search_support::request())
        .await;

    assert!(result.is_ok(), "{result:?}");
    assert_resent_with_new_token(&requests_to(&server, RESPONSES_PATH).await);
}

// Given: 回復後の hosted search も 401 / When: search / Then: 本文を伏せた 401 を返し、再送は 1 回だけ
#[tokio::test]
async fn hosted_search_second_unauthorized_is_sanitized_and_final() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    for access in ["access-old", "access-new"] {
        mount_status(
            &server,
            RESPONSES_PATH,
            access,
            ResponseTemplate::new(401).set_body_string("secret-body"),
        )
        .await;
    }

    let error = client(&server, seeded_store(), None)
        .hosted_web_search()
        .expect("codex searches")
        .search(&ProviderAuth::new(""), &web_search_support::request())
        .await
        .expect_err("second 401 is final");

    assert!(
        matches!(&error, HostedWebSearchError::Provider(inner) if inner.status() == Some(401)),
        "{error:?}"
    );
    assert!(!format!("{error:?}").contains("secret-body"));
    assert_eq!(requests_to(&server, RESPONSES_PATH).await.len(), 2);
}

fn image_response() -> ResponseTemplate {
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 3)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    ResponseTemplate::new(200).set_body_json(json!({
        "created": 123,
        "background": "transparent",
        "data": [{"b64_json": STANDARD.encode(png.into_inner()), "generation_id": "gen-1"}],
        "usage": {"input_tokens": 42, "output_tokens": 7, "total_tokens": 49,
            "input_tokens_details": {"text_tokens": 12, "image_tokens": 30}}
    }))
}

// Given: image generation が 401 / When: generate / Then: 回復して同一 body で再送し成功する
#[tokio::test]
async fn image_generation_recovers_from_unauthorized() {
    let server = MockServer::start().await;
    let endpoint = "/backend-api/codex/images/generations";
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    mount_status(&server, endpoint, "access-old", unauthorized()).await;
    mount_status(&server, endpoint, "access-new", image_response()).await;
    let client = client(&server, seeded_store(), None);

    let result = client
        .image_generation()
        .expect("codex generates images")
        .generate(
            &ProviderAuth::new(""),
            &ImageGenerationRequest {
                model: "gpt-image-2".into(),
                prompt: "draw a tree".into(),
                ..Default::default()
            },
        )
        .await;

    assert!(result.is_ok(), "{result:?}");
    assert_resent_with_new_token(&requests_to(&server, endpoint).await);
}

// Given: 401 後に回復する send / When: 観測イベントを読む / Then: 1 回目は Auth 失敗、2 回目は新しい request_id の attempt
#[tokio::test]
async fn unauthorized_attempt_is_recorded_before_a_new_attempt() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(&server, RESPONSES_PATH, "access-new", sse(CHAT_SSE)).await;
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();

    client(&server, seeded_store(), Some(bus))
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect("send recovers");

    let mut timeline = Vec::new();
    loop {
        let event = events.recv().await.expect("event bus stays open");
        let EventKind::Provider(provider) = event.kind else {
            continue;
        };
        match provider {
            ProviderEvent::RequestStarted { request_id, .. } => {
                timeline.push(("started", request_id, None));
            }
            ProviderEvent::RequestFailed {
                request_id,
                failure,
                ..
            } => timeline.push(("failed", request_id, Some(failure))),
            ProviderEvent::RequestCompleted { request_id, .. } => {
                timeline.push(("completed", request_id, None));
                break;
            }
            _ => {}
        }
    }
    let phases: Vec<_> = timeline.iter().map(|(phase, ..)| *phase).collect();
    assert_eq!(phases, ["started", "failed", "started", "completed"]);
    assert_eq!(timeline[1].2, Some(ProviderFailureKind::Auth));
    assert_eq!(timeline[0].1, timeline[1].1);
    assert_eq!(timeline[2].1, timeline[3].1);
    assert_ne!(timeline[0].1, timeline[2].1);
}
