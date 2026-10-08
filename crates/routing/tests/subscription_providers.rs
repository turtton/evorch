use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig,
};
use routing::{ComposeDeps, MapEnv, compose_providers};
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use std::sync::Arc;

#[test]
fn subscription_composition_does_not_treat_the_token_bundle_as_a_request_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::open(dir.path()).unwrap());
    for (name, kind, protocol, url) in [
        (
            "claude",
            ProviderTypeConfig::AnthropicSubscription,
            ApiProtocolConfig::AnthropicMessages,
            "https://api.anthropic.com/v1",
        ),
        (
            "cursor",
            ProviderTypeConfig::Cursor,
            ApiProtocolConfig::CursorAgent,
            "https://api2.cursor.sh",
        ),
    ] {
        // A malformed bundle cannot stop composition or be exposed as request auth.
        store
            .set(name, &Secret::from("SENTINEL-token-bundle".to_owned()))
            .unwrap();
        let mut config = Config::default();
        config.providers.insert(
            name.into(),
            ProviderProfileConfig {
                provider_type: kind,
                api_protocol: protocol,
                base_url: url.into(),
                credential: CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: name.into(),
                },
                models: vec![config::ModelEntryConfig::enabled("model")],
                default_model: "model".into(),
                excluded_models: vec![],
            },
        );
        let composed = compose_providers(
            &config,
            ComposeDeps {
                credential_store: store.clone(),
                env: Arc::new(MapEnv::default()),
                event_bus: None,
                catalog: model::ModelCatalog::new(),
                factory: routing::factory::FactoryOptions::default(),
            },
        )
        .unwrap();
        let provider = composed.provider(name).unwrap();
        assert!(provider.auth.api_key.is_empty());
        assert!(!format!("{composed:?}").contains("SENTINEL-token-bundle"));
        assert!(provider.client.capabilities().streaming);
        assert!(provider.client.capabilities().tool_use);
    }
}

#[test]
fn corrupt_stored_claude_credentials_are_rejected_without_echoing_the_secret() {
    use providers::provider::claude::ClaudeTokenStore;
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::open(dir.path()).unwrap());
    store
        .set(
            "claude",
            &Secret::from(r#"{"expires_at":"SENTINEL-private-value"}"#.to_owned()),
        )
        .unwrap();
    let adapter =
        routing::factory::CredentialStoreClaudeTokenStore::new(store.clone(), "claude".into());
    let error = adapter.load().unwrap_err();
    assert!(!error.to_string().contains("SENTINEL-private-value"));
    let second = routing::factory::CredentialStoreClaudeTokenStore::new(store, "claude".into());
    let first_lock = adapter.refresh_lock();
    assert!(Arc::ptr_eq(&first_lock, &second.refresh_lock()));
}
