use super::auth::jwt;
use super::*;
use providers::ProviderError;
use providers::provider::codex::{oauth::DeviceAuthClient, session::CodexSessionManager};
use wiremock::matchers::body_json;

fn config(server: &MockServer) -> QuotaConfig {
    QuotaConfig {
        wham_endpoint: format!("{}/usage", server.uri()),
        auth_base_url: server.uri(),
        ..QuotaConfig::default()
    }
}
fn seed(expired: bool) -> Arc<InMemoryTokenStore> {
    let store = store();
    store
        .save(&TokenBundle {
            access_token: jwt(if expired { 1 } else { 4_000_000_000 }),
            refresh_token: "refresh-old".into(),
            id_token: jwt(1),
        })
        .unwrap();
    store
}
fn rotated() -> TokenBundle {
    TokenBundle {
        access_token: "access-new".into(),
        refresh_token: "refresh-new".into(),
        id_token: jwt(4_000_000_000),
    }
}
async fn refresh(server: &MockServer, status: u16, reason: &str) {
    let response = if status == 200 {
        // Deliberately omit id_token: the expired account-selector JWT remains.
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": jwt(4_000_000_001), "refresh_token": "refresh-new"
        }))
    } else {
        ResponseTemplate::new(status).set_body_string("SECRET")
    };
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(
            json!({"client_id": providers::provider::codex::oauth::CODEX_CLIENT_ID,
            "grant_type":"refresh_token", "refresh_token":"refresh-old", "refresh_reason":reason}),
        ))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}
async fn wham(server: &MockServer, access: &str, status: u16, count: u64) {
    Mock::given(method("GET"))
        .and(path("/usage"))
        .and(header("authorization", format!("Bearer {access}")))
        .and(header("chatgpt-account-id", "account-1"))
        .respond_with(ResponseTemplate::new(status).set_body_json(usage()))
        .expect(count)
        .mount(server)
        .await;
}

#[tokio::test]
async fn proactive_refresh_without_id_token_preserves_account_selector() {
    let server = MockServer::start().await;
    let store = seed(true);
    refresh(&server, 200, "proactive_expiry").await;
    wham(&server, &jwt(4_000_000_001), 200, 1).await;
    let mut client = CodexQuotaClient::new_for_account(config(&server), store.clone()).unwrap();
    assert!(!client.fetch_quota().await.unwrap().stale);
    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.access_token, jwt(4_000_000_001));
    assert_eq!(saved.refresh_token, "refresh-new");
    assert_eq!(saved.id_token, jwt(1));
}

#[tokio::test]
async fn unauthorized_adopts_another_actors_rotated_store_without_refresh() {
    let server = MockServer::start().await;
    let store = seed(false);
    let actor = store.clone();
    Mock::given(method("GET"))
        .and(header(
            "authorization",
            format!("Bearer {}", jwt(4_000_000_000)),
        ))
        .respond_with(move |_: &wiremock::Request| {
            actor.save(&rotated()).unwrap();
            ResponseTemplate::new(401)
        })
        .expect(1)
        .mount(&server)
        .await;
    wham(&server, "access-new", 200, 1).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let mut client = CodexQuotaClient::new_for_account(config(&server), store).unwrap();
    assert!(!client.fetch_quota().await.unwrap().stale);
}

#[tokio::test]
async fn unauthorized_refreshes_and_retries_once() {
    let server = MockServer::start().await;
    refresh(&server, 200, "unauthorized_recovery").await;
    wham(&server, &jwt(4_000_000_000), 401, 1).await;
    wham(&server, &jwt(4_000_000_001), 200, 1).await;
    let mut client = CodexQuotaClient::new_for_account(config(&server), seed(false)).unwrap();
    assert!(!client.fetch_quota().await.unwrap().stale);
}

// Each terminal authentication failure suppresses OAuth on later polls until
// the stored bundle changes, not merely until the poll/backoff timer expires.
#[tokio::test]
async fn terminal_auth_failures_are_latched_across_polls_and_login_unblocks() {
    for (expired, status) in [(true, 400), (false, 403), (false, 200)] {
        let server = MockServer::start().await;
        let store = seed(expired);
        refresh(
            &server,
            status,
            if expired {
                "proactive_expiry"
            } else {
                "unauthorized_recovery"
            },
        )
        .await;
        if !expired {
            wham(&server, &jwt(4_000_000_000), 401, 1).await;
        }
        if status == 200 {
            wham(&server, &jwt(4_000_000_001), 401, 1).await;
        }
        let mut client = CodexQuotaClient::new_for_account(config(&server), store.clone()).unwrap();
        for _ in 0..2 {
            let error = client.fetch_quota().await.unwrap_err();
            assert!(
                matches!(error, QuotaError::ReauthenticationRequired),
                "{error:?}"
            );
            assert!(!format!("{error:?}").contains("SECRET"));
            tokio::time::pause();
            tokio::time::advance(client.poll_interval()).await;
            tokio::time::resume();
        }
        store.save(&rotated()).unwrap();
        wham(&server, "access-new", 200, 1).await;
        assert!(!client.fetch_quota().await.unwrap().stale);
    }
}

#[tokio::test]
async fn transient_refresh_and_wham_errors_do_not_require_login() {
    for expired in [true, false] {
        for status in [429, 500, 503] {
            let server = MockServer::start().await;
            refresh(
                &server,
                status,
                if expired {
                    "proactive_expiry"
                } else {
                    "unauthorized_recovery"
                },
            )
            .await;
            if !expired {
                wham(&server, &jwt(4_000_000_000), 401, 1).await;
            }
            let mut client =
                CodexQuotaClient::new_for_account(config(&server), seed(expired)).unwrap();
            let error = client.fetch_quota().await.unwrap_err();
            assert!(
                matches!(error, QuotaError::HttpStatus(s) if s == status),
                "{error:?}"
            );
            assert!(!format!("{error:?}").contains("SECRET"));
        }
    }
    for status in [403, 429, 500] {
        let server = MockServer::start().await;
        wham(&server, &jwt(4_000_000_000), status, 1).await;
        let mut client = CodexQuotaClient::new_for_account(config(&server), seed(false)).unwrap();
        assert!(
            matches!(client.fetch_quota().await, Err(QuotaError::HttpStatus(s)) if s == status)
        );
    }
}

struct BrokenStore;
impl CodexTokenStore for BrokenStore {
    fn load(&self) -> Result<Option<TokenBundle>, ProviderError> {
        Err(ProviderError::Request("SECRET".into()))
    }
    fn save(&self, _: &TokenBundle) -> Result<(), ProviderError> {
        unreachable!()
    }
}
#[tokio::test]
async fn invalid_credentials_and_store_errors_are_sanitized() {
    let server = MockServer::start().await;
    let malformed = seed(false);
    let mut token = malformed.load().unwrap().unwrap();
    token.id_token = "SECRET".into();
    malformed.save(&token).unwrap();
    for store in [
        malformed as Arc<dyn CodexTokenStore>,
        Arc::new(BrokenStore),
        Arc::new(InMemoryTokenStore::new()),
    ] {
        let mut client = CodexQuotaClient::new_for_account(config(&server), store).unwrap();
        let error = client.fetch_quota().await.unwrap_err();
        assert!(matches!(error, QuotaError::Credentials));
        assert!(!format!("{error:?}").contains("SECRET"));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

// The adapter parks save while holding the account refresh lock. Polling the
// other manager once proves it cannot even read the old store in that interval.
struct ParkedStore {
    inner: Arc<InMemoryTokenStore>,
    loads: std::sync::atomic::AtomicUsize,
    parked: tokio::sync::Notify,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
struct Adapter(Arc<ParkedStore>);
impl CodexTokenStore for Adapter {
    fn load(&self) -> Result<Option<TokenBundle>, ProviderError> {
        self.0
            .loads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.0.inner.load()
    }
    fn save(&self, bundle: &TokenBundle) -> Result<(), ProviderError> {
        self.0.parked.notify_one();
        self.0.release.lock().unwrap().recv().unwrap();
        self.0.inner.save(bundle)
    }
}

async fn quota_and_chat_managers_serialize_rotation(expired: bool) {
    use futures_util::FutureExt;
    use std::sync::atomic::Ordering;
    let server = MockServer::start().await;
    let (release, receiver) = std::sync::mpsc::channel();
    let shared = Arc::new(ParkedStore {
        inner: seed(expired),
        loads: 0.into(),
        parked: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(receiver),
    });
    let quota_store = Arc::new(Adapter(shared.clone()));
    let chat_store = Arc::new(Adapter(shared.clone()));
    let chat = CodexSessionManager::new(
        chat_store,
        DeviceAuthClient::with_default_http(server.uri()).unwrap(),
    );
    let rejected = if expired {
        None
    } else {
        Some(chat.current().await.unwrap())
    };
    refresh(
        &server,
        200,
        if expired {
            "proactive_expiry"
        } else {
            "unauthorized_recovery"
        },
    )
    .await;
    if !expired {
        wham(&server, &jwt(4_000_000_000), 401, 1).await;
    }
    wham(&server, &jwt(4_000_000_001), 200, 1).await;
    let mut quota = CodexQuotaClient::new_for_account(config(&server), quota_store).unwrap();
    let fetching = tokio::spawn(async move { quota.fetch_quota().await });
    shared.parked.notified().await;
    let loads = shared.loads.load(Ordering::SeqCst);
    let mut other = Box::pin(async {
        if let Some(token) = &rejected {
            chat.refresh_after_unauthorized(token).await
        } else {
            chat.current().await
        }
    });
    let pending = (&mut other).now_or_never().is_none();
    let unchanged = shared.loads.load(Ordering::SeqCst) == loads;
    // Always release before asserting so even a deliberately broken lock
    // cannot leave a synchronous save parked during runtime teardown.
    release.send(()).unwrap();
    assert!(
        pending && unchanged,
        "the other manager must wait before reloading the store"
    );
    other.await.unwrap();
    assert!(!fetching.await.unwrap().unwrap().stale);
    let saved = shared.inner.load().unwrap().unwrap();
    assert_eq!(saved.refresh_token, "refresh-new");
    assert_eq!(saved.access_token, jwt(4_000_000_001));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_and_chat_managers_serialize_proactive_rotation() {
    quota_and_chat_managers_serialize_rotation(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_and_chat_managers_serialize_unauthorized_rotation() {
    quota_and_chat_managers_serialize_rotation(false).await;
}

#[tokio::test]
async fn passive_fallback_uses_expired_id_only_as_an_account_selector() {
    let server = MockServer::start().await;
    wham(&server, &jwt(4_000_000_000), 200, 1).await;
    let mut client = CodexQuotaClient::new(
        missing_config(format!("{}/usage", server.uri())),
        seed(false),
    )
    .unwrap();
    assert!(!client.fetch_quota().await.unwrap().stale);
}

#[tokio::test]
async fn transport_failures_in_refresh_and_wham_are_not_authentication_failures() {
    use tokio::io::AsyncReadExt;
    for expired in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut byte = [0];
            connection.read_exact(&mut byte).await.unwrap();
            // Close without an HTTP response, after the request actually starts.
        });
        let config = QuotaConfig {
            auth_base_url: endpoint.clone(),
            wham_endpoint: endpoint,
            ..QuotaConfig::default()
        };
        let mut client = CodexQuotaClient::new_for_account(config, seed(expired)).unwrap();
        let error = client.fetch_quota().await.unwrap_err();
        peer.await.unwrap();
        assert!(matches!(error, QuotaError::HttpTransport), "{error:?}");
    }
}

struct UnsavableStore(Arc<InMemoryTokenStore>);
impl CodexTokenStore for UnsavableStore {
    fn load(&self) -> Result<Option<TokenBundle>, ProviderError> {
        self.0.load()
    }
    fn save(&self, _: &TokenBundle) -> Result<(), ProviderError> {
        Err(ProviderError::Request("SECRET".into()))
    }
}
#[tokio::test]
async fn refresh_save_failure_is_credentials_and_does_not_send_wham() {
    let server = MockServer::start().await;
    refresh(&server, 200, "proactive_expiry").await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let mut client =
        CodexQuotaClient::new_for_account(config(&server), Arc::new(UnsavableStore(seed(true))))
            .unwrap();
    assert!(matches!(
        client.fetch_quota().await,
        Err(QuotaError::Credentials)
    ));
}

#[tokio::test]
async fn account_scoped_wham_does_not_follow_redirects() {
    let server = MockServer::start().await;
    Mock::given(path("/usage"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/redirect-target", server.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/redirect-target"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let mut client = CodexQuotaClient::new_for_account(config(&server), seed(false)).unwrap();
    assert!(matches!(
        client.fetch_quota().await,
        Err(QuotaError::HttpStatus(302))
    ));
}

#[tokio::test]
async fn shared_default_lock_is_stable_and_valid_cache_does_not_acquire_it() {
    use futures_util::FutureExt;
    let store = seed(false);
    let lock = store.refresh_lock();
    assert!(Arc::ptr_eq(&lock, &store.refresh_lock()));
    assert!(Arc::ptr_eq(
        &lock,
        &InMemoryTokenStore::new().refresh_lock()
    ));
    let session = CodexSessionManager::new(
        store,
        DeviceAuthClient::with_default_http("http://unused.invalid").unwrap(),
    );
    session.current().await.unwrap();
    let _guard = lock.lock().await;
    assert!(
        session.current().now_or_never().unwrap().is_ok(),
        "valid cache must not wait for the shared lock"
    );
}
