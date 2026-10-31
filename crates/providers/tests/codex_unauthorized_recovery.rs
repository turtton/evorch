//! Codex が 401 で拒否した認証情報を 1 回だけ回復して再送する契約。
//!
//! 再送は同一 wire body で行う (prompt cache の prefix を変えない)。

use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use event_bus::{EventBus, EventKind, ProviderEvent, ProviderFailureKind};
use futures_util::FutureExt;
use providers::provider::codex::oauth::CODEX_CLIENT_ID;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{
    ChatRequest, ContentBlock, HostedWebSearchError, ImageGenerationError, ImageGenerationRequest,
    Message, ProviderAuth, ProviderClient, ProviderError, Role,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Barrier;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
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
    client_at(&server.uri(), store, bus)
}

fn client_at(
    uri: &str,
    store: Arc<dyn CodexTokenStore>,
    bus: Option<Arc<EventBus>>,
) -> CodexClient {
    CodexClient::with_config(
        CodexConfig {
            base_url: uri.to_owned(),
            auth_base_url: uri.to_owned(),
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

/// 旧 token の 401 を、両リクエストが届くまで返さない最小の HTTP サーバー。
///
/// wiremock の応答は同期で、保留すると単一スレッドのサーバーごと止まるため使えない。
struct GatedBackend {
    uri: String,
    refreshes: Arc<AtomicUsize>,
}

async fn spawn_gated_backend() -> GatedBackend {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let uri = format!("http://{}", listener.local_addr().expect("local address"));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let both_arrived = Arc::new(Barrier::new(2));
    let counter = refreshes.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            tokio::spawn(serve(stream, both_arrived.clone(), counter.clone()));
        }
    });
    GatedBackend { uri, refreshes }
}

async fn serve(mut stream: TcpStream, both_arrived: Arc<Barrier>, refreshes: Arc<AtomicUsize>) {
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    let (head_len, content_length) = loop {
        let read = stream.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "connection closed before the request completed");
        received.extend_from_slice(&chunk[..read]);
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&received[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .map_or(0, |value| value.trim().parse::<usize>().expect("length"));
            break (end + 4, length);
        }
    };
    while received.len() < head_len + content_length {
        let read = stream.read(&mut chunk).await.expect("read body");
        assert!(read > 0, "connection closed before the body completed");
        received.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&received[..head_len]).to_lowercase();
    let (status, content_type, body) = if head.starts_with("post /oauth/token") {
        refreshes.fetch_add(1, Ordering::SeqCst);
        let body = json!({
            "access_token": "access-new",
            "refresh_token": "refresh-new",
            "id_token": jwt(now_unix() + 7_200, "acc-123")
        });
        ("200 OK", "application/json", body.to_string())
    } else if head.starts_with("get /models") {
        // The client reads the model catalog once before its first generation.
        ("200 OK", "application/json", r#"{"models":[]}"#.to_owned())
    } else if head.contains("authorization: bearer access-old") {
        both_arrived.wait().await;
        ("401 Unauthorized", "text/plain", "token expired".to_owned())
    } else {
        ("200 OK", "text/event-stream", CHAT_SSE.to_owned())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("respond");
    stream.shutdown().await.expect("shutdown");
}

/// 最初の `save` を、テストが解放するまで保留する store。
///
/// `save` は refresh 中 (session のロックを保持している間) に呼ばれるため、
/// 保留中は refresh が進行中でロックが取られたままであることを保証できる。
struct ParkedSaveStore {
    inner: InMemoryTokenStore,
    parked: UnboundedSender<()>,
    release: Mutex<Receiver<()>>,
}

impl CodexTokenStore for ParkedSaveStore {
    fn load(&self) -> Result<Option<TokenBundle>, ProviderError> {
        self.inner.load()
    }

    fn save(&self, bundle: &TokenBundle) -> Result<(), ProviderError> {
        self.parked.send(()).expect("test is waiting for the park");
        self.release
            .lock()
            .unwrap()
            .recv()
            .expect("test releases the save");
        self.inner.save(bundle)
    }
}

// Given: 旧 token を持つ 2 リクエストが両方 401 を受け、refresh が保存で保留されている
// When: その間に別の current() を走らせ、保留を解放する
// Then: 別の current() は保留中は完了せず解放後に完了し、refresh は 1 回で両方の再送が成功する
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_unauthorized_responses_refresh_once() {
    let backend = spawn_gated_backend().await;
    let (parked, mut parked_signal) = unbounded_channel();
    let (release, release_signal) = std::sync::mpsc::channel();
    let store = Arc::new(ParkedSaveStore {
        inner: Arc::try_unwrap(seeded_store()).ok().expect("sole owner"),
        parked,
        release: Mutex::new(release_signal),
    });
    let client = Arc::new(client_at(&backend.uri, store, None));
    let auth = ProviderAuth::new("");
    let preload = request();
    client
        .resolve_responses_scope(&auth, &preload)
        .await
        .expect("both requests start from the cached old token");
    let sends: Vec<_> = (0..2)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.send(&ProviderAuth::new(""), &request()).await })
        })
        .collect();

    parked_signal
        .recv()
        .await
        .expect("a refresh reached the store");
    let mut concurrent_current = Box::pin(client.resolve_responses_scope(&auth, &preload));
    assert!(
        (&mut concurrent_current).now_or_never().is_none(),
        "current() must wait for the refresh holding the session lock"
    );
    release.send(()).expect("release the parked save");

    let scope = concurrent_current.await.expect("current() completes");
    assert!(scope.is_some());
    for send in sends {
        let result = send.await.expect("send task");
        assert!(result.is_ok(), "{result:?}");
    }
    assert_eq!(backend.refreshes.load(Ordering::SeqCst), 1);
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

fn image_request() -> ImageGenerationRequest {
    ImageGenerationRequest {
        model: "gpt-image-2".into(),
        prompt: "draw a tree".into(),
        ..Default::default()
    }
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
        .generate(&ProviderAuth::new(""), &image_request())
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

// Given: 回復で account が変わる hosted search / When: search / Then: 再送せず ResponsesScopeChanged を返す
#[tokio::test]
async fn hosted_search_account_change_is_not_resent() {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-other"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;

    let error = client(&server, seeded_store(), None)
        .hosted_web_search()
        .expect("codex searches")
        .search(&ProviderAuth::new(""), &web_search_support::request())
        .await
        .expect_err("account change must not be replayed");

    assert!(
        matches!(
            error,
            HostedWebSearchError::Provider(ProviderError::ResponsesScopeChanged { .. })
        ),
        "{error:?}"
    );
    assert_eq!(requests_to(&server, RESPONSES_PATH).await.len(), 1);
}

// Given: 回復で account が変わる image generation / When: generate / Then: 再送せず ResponsesScopeChanged を返す
#[tokio::test]
async fn image_generation_account_change_is_not_resent() {
    let server = MockServer::start().await;
    let endpoint = "/backend-api/codex/images/generations";
    mount_refresh(&server, refreshed_response("acc-other"), 1).await;
    mount_status(&server, endpoint, "access-old", unauthorized()).await;

    let error = client(&server, seeded_store(), None)
        .image_generation()
        .expect("codex generates images")
        .generate(&ProviderAuth::new(""), &image_request())
        .await
        .expect_err("account change must not be replayed");

    assert!(
        matches!(
            error,
            ImageGenerationError::Provider(ProviderError::ResponsesScopeChanged { .. })
        ),
        "{error:?}"
    );
    assert_eq!(requests_to(&server, endpoint).await.len(), 1);
}

const REMOTE_SECRET: &str = "sk-remote-secret-marker-9f31";

/// 2 回目の 401 本文 (`REMOTE_SECRET`) が、返却エラーと観測イベントのどちらにも出ないこと。
async fn assert_second_unauthorized_hides_remote_body(compaction: bool) {
    let server = MockServer::start().await;
    mount_refresh(&server, refreshed_response("acc-123"), 1).await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;
    mount_status(
        &server,
        RESPONSES_PATH,
        "access-new",
        ResponseTemplate::new(401).set_body_string(format!("invalid token {REMOTE_SECRET}")),
    )
    .await;
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    let client = client(&server, seeded_store(), Some(bus));

    let error = if compaction {
        client
            .compactor()
            .expect("codex compacts")
            .compact(&request())
            .await
            .expect_err("second 401 is final")
    } else {
        client
            .send(&ProviderAuth::new(""), &request())
            .await
            .expect_err("second 401 is final")
    };

    assert!(
        matches!(error, ProviderError::Http { status: 401, .. }),
        "{error:?}"
    );
    assert!(!format!("{error:?} {error}").contains(REMOTE_SECRET));
    let mut failures = 0;
    while failures < 2 {
        let event = events.recv().await.expect("event bus stays open");
        if matches!(
            event.kind,
            EventKind::Provider(ProviderEvent::RequestFailed { .. })
        ) {
            failures += 1;
        }
        let json = serde_json::to_string(&event).expect("events serialize");
        assert!(!json.contains(REMOTE_SECRET), "{json}");
    }
}

// Given: 回復後の再送も秘密を含む 401 / When: send / Then: 返却エラーと観測に遠隔の本文が出ない
#[tokio::test]
async fn second_unauthorized_hides_the_remote_body_for_chat() {
    assert_second_unauthorized_hides_remote_body(false).await;
}

// Given: 回復後の再送も秘密を含む 401 / When: compact / Then: 返却エラーと観測に遠隔の本文が出ない
#[tokio::test]
async fn second_unauthorized_hides_the_remote_body_for_compaction() {
    assert_second_unauthorized_hides_remote_body(true).await;
}

// Given: refresh が 408 (一時障害) / When: send / Then: 再認証を要求する 401 にせず、状態を保ち再送もしない
#[tokio::test]
async fn transient_refresh_failure_is_not_reported_as_reauthentication() {
    let server = MockServer::start().await;
    mount_refresh(
        &server,
        ResponseTemplate::new(408).set_body_string(REMOTE_SECRET),
        1,
    )
    .await;
    mount_status(&server, RESPONSES_PATH, "access-old", unauthorized()).await;

    let error = client(&server, seeded_store(), None)
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect_err("a transient refresh failure surfaces");

    assert!(
        matches!(&error, ProviderError::Http { status: 408, body } if !body.contains("re-authenticate")),
        "{error:?}"
    );
    assert!(!format!("{error:?}").contains(REMOTE_SECRET));
    assert_eq!(requests_to(&server, RESPONSES_PATH).await.len(), 1);
}
