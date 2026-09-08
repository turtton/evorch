use std::sync::Arc;

use config::{Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig};
use providers::provider::codex::oauth::DeviceAuthClient;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};

use super::{ProviderCodexAuthBackend, codex_credential_account};
use crate::model::codex_auth::{CodexAuthBackend, CodexAuthModel, CodexAuthSummary};

const DUMMY_JWT: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjE4OTM0NTYwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2MtMTIzIn19.sig";

struct FailingLoadStore;

impl CodexTokenStore for FailingLoadStore {
    fn load(&self) -> Result<Option<TokenBundle>, providers::ProviderError> {
        Err(providers::ProviderError::Http {
            status: 403,
            body: "sentinel-access-abc".into(),
        })
    }

    fn save(&self, _: &TokenBundle) -> Result<(), providers::ProviderError> {
        unreachable!("load-only fixture")
    }
}

#[test]
fn failed_store_load_never_exposes_error_body() {
    // Given: a store error containing secret material.
    let backend = Arc::new(backend(Arc::new(FailingLoadStore)));
    // When: the adapter loads the initial GUI state.
    let model = CodexAuthModel::with_backend(backend, "codex");
    // Then: even the state's Debug representation is safe.
    assert!(matches!(
        model.state,
        crate::model::codex_auth::CodexAuthState::Failed { .. }
    ));
    assert!(!format!("{:?}", model.state).contains("sentinel-access-abc"));
}

fn backend(store: Arc<dyn CodexTokenStore>) -> ProviderCodexAuthBackend {
    ProviderCodexAuthBackend::new(
        DeviceAuthClient::with_default_http("https://auth.invalid").expect("HTTP client"),
        store,
    )
}

#[test]
fn refresh_stays_unauthenticated_when_login_save_fails() {
    use crate::model::codex_auth::{CodexAuthError, CodexAuthState};
    use std::io::{Read, Write};
    // Given: a real OAuth wire flow and a file store with an unavailable directory.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    let server = std::thread::spawn(move || {
        for body in [
            r#"{"device_auth_id":"device","user_code":"ABCD-1234","interval":"0"}"#.to_owned(),
            r#"{"authorization_code":"code","code_verifier":"verifier"}"#.to_owned(),
            serde_json::json!({"access_token":"sentinel-access-abc","refresh_token":"refresh-secret","id_token":DUMMY_JWT}).to_string(),
        ] {
            let (mut socket, _) = listener.accept().expect("request");
            socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).expect("timeout");
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).expect("headers");
                request.push(byte[0]);
            }
            let headers = String::from_utf8(request).expect("HTTP headers");
            let length: usize = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().expect("length"))
            }).expect("content length");
            socket.read_exact(&mut vec![0; length]).expect("body");
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response");
        }
    });
    let parent = tempfile::tempdir().expect("temp directory");
    let dir = parent.path().join("store");
    let backup = parent.path().join("backup");
    let store = Arc::new(sandbox::FileCredentialStore::open(&dir).expect("store"));
    let backend = Arc::new(
        ProviderCodexAuthBackend::production(store, "codex".into(), &url).expect("backend"),
    );
    let mut model = CodexAuthModel::with_backend(backend, "codex");
    std::fs::rename(&dir, &backup).expect("move directory");
    // When: login completes the exchange but cannot commit the credentials.
    model.start();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while model.is_authenticating() {
        model.poll();
        assert!(std::time::Instant::now() < deadline, "login deadline");
        std::thread::yield_now();
    }
    server.join().expect("server");
    // Then: the error is safe and reloading cannot authenticate from residue.
    assert_eq!(
        model.state,
        CodexAuthState::Failed {
            failure: CodexAuthError::StoreUnavailable
        }
    );
    assert!(!format!("{:?}", model.state).contains("sentinel-access-abc"));
    model.refresh_from_store();
    assert_eq!(model.state, CodexAuthState::Unauthenticated);
}

#[test]
fn load_summary_is_none_for_empty_store() {
    // Given
    let backend = backend(Arc::new(InMemoryTokenStore::new()));
    // When
    let summary = backend.load_summary();
    // Then
    assert_eq!(summary, Ok(None));
}

#[test]
fn load_summary_exposes_only_expiry_never_token_material() {
    // Given
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(&TokenBundle {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            id_token: DUMMY_JWT.into(),
        })
        .expect("seed tokens");
    let backend = Arc::new(backend(store));
    // When
    let summary = backend.load_summary().expect("load summary");
    let model = CodexAuthModel::with_backend(backend, "codex");
    // Then
    assert_eq!(
        summary,
        Some(CodexAuthSummary {
            expires_at_unix: Some(1_893_456_000),
        })
    );
    let debug = format!("{summary:?} {model:?}");
    for secret in ["access-secret", "refresh-secret", "id-secret", DUMMY_JWT] {
        assert!(!debug.contains(secret));
    }
}

#[test]
fn load_summary_with_unparseable_id_token_fails_closed() {
    // Given
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(&TokenBundle {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            id_token: "garbage".into(),
        })
        .expect("seed tokens");
    // When
    let summary = backend(store).load_summary();
    // Then
    assert_eq!(
        summary,
        Err(crate::model::codex_auth::CodexAuthError::StoreUnavailable)
    );
}

#[test]
fn codex_credential_account_picks_first_openai_codex_keyring_profile() {
    // Given
    let mut config = Config::default();
    config
        .providers
        .insert("a-anthropic".into(), ProviderProfileConfig::default());
    config.providers.insert(
        "z-codex".into(),
        ProviderProfileConfig {
            provider_type: ProviderTypeConfig::OpenAiCodex,
            credential: CredentialRefConfig::Keyring {
                service: "evorch".into(),
                account: "work".into(),
            },
            ..ProviderProfileConfig::default()
        },
    );
    // When
    let account = codex_credential_account(&config);
    // Then
    assert_eq!(account.as_deref(), Some("work"));
}

#[test]
fn codex_credential_account_ignores_env_credentials_and_missing_profiles() {
    // Given
    let empty = Config::default();
    let mut env = Config::default();
    env.providers.insert(
        "codex".into(),
        ProviderProfileConfig {
            provider_type: ProviderTypeConfig::OpenAiCodex,
            ..ProviderProfileConfig::default()
        },
    );
    // When / Then
    for config in [empty, env] {
        assert_eq!(codex_credential_account(&config), None);
    }
}

#[test]
fn production_builds_backend_over_file_credential_store() {
    // Given
    let dir = tempfile::tempdir().expect("temporary credentials");
    let store = Arc::new(sandbox::FileCredentialStore::open(dir.path()).expect("file store"));
    // When
    let backend =
        ProviderCodexAuthBackend::production(store, "codex".into(), "https://auth.invalid")
            .expect("production backend");
    // Then
    assert_eq!(backend.load_summary(), Ok(None));
}
