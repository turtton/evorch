use std::sync::Arc;

use config::{Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig};
use providers::provider::codex::oauth::DeviceAuthClient;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};

use super::{ProviderCodexAuthBackend, codex_credential_account};
use crate::model::codex_auth::{CodexAuthBackend, CodexAuthModel, CodexAuthSummary};

const DUMMY_JWT: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjE4OTM0NTYwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2MtMTIzIn19.sig";

fn backend(store: Arc<dyn CodexTokenStore>) -> ProviderCodexAuthBackend {
    ProviderCodexAuthBackend::new(
        DeviceAuthClient::with_default_http("https://auth.invalid").expect("HTTP client"),
        store,
    )
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
fn load_summary_with_unparseable_id_token_has_no_expiry() {
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
        Ok(Some(CodexAuthSummary {
            expires_at_unix: None
        }))
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
