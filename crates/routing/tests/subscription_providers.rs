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

#[tokio::test]
async fn cursor_api_key_profile_reexchanges_stored_key_before_native_model_discovery() {
    use providers::provider::cursor::{CursorTokenBundle, CursorTokenStore};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::open(dir.path()).unwrap());
    let tokens =
        routing::factory::CredentialStoreCursorTokenStore::new(store.clone(), "cursor".into());
    tokens
        .save(&CursorTokenBundle {
            access_token: "expired-access-secret".into(),
            refresh_token: String::new(),
            api_key: Some("cursor-key-secret".into()),
            expires_at: 0,
            account_id: None,
            email: None,
        })
        .unwrap();
    Mock::given(path("/auth/exchange_user_api_key"))
        .and(method("POST"))
        .and(header("authorization", "Bearer cursor-key-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"accessToken":"exchanged-access-secret"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/agent.v1.AgentService/GetUsableModels"))
        .and(header("authorization", "Bearer exchanged-access-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(vec![10, 9, 10, 7, b'm', b'o', b'd', b'e', b'l', b'-', b'1']),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.providers.insert(
        "cursor".into(),
        ProviderProfileConfig {
            provider_type: ProviderTypeConfig::Cursor,
            api_protocol: ApiProtocolConfig::CursorAgent,
            base_url: server.uri(),
            credential: CredentialRefConfig::Keyring {
                service: "evorch".into(),
                account: "cursor".into(),
            },
            models: vec![config::ModelEntryConfig::enabled("model-1")],
            default_model: "model-1".into(),
            ..Default::default()
        },
    );
    let composed = compose_providers(
        &config,
        ComposeDeps {
            credential_store: store,
            env: Arc::new(MapEnv::default()),
            event_bus: None,
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions {
                auth_base_url_override: Some(server.uri()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let provider = composed.provider("cursor").unwrap();
    assert!(provider.auth.api_key.is_empty());
    let models = provider
        .client
        .list_models(&provider.auth)
        .await
        .unwrap()
        .unwrap();
    assert!(models.contains(&"model-1".to_owned()));
    let saved = tokens.load().unwrap().unwrap();
    assert_eq!(saved.api_key.as_deref(), Some("cursor-key-secret"));
    assert_eq!(saved.access_token, "exchanged-access-secret");
    assert!(!format!("{composed:?} {saved:?}").contains("cursor-key-secret"));
}
