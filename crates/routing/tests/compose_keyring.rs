use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use config::{Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig};
use routing::{ComposeDeps, MapEnv, compose_providers};
use sandbox::CredentialError;
use sandbox::credential::{CredentialStore, Secret};

#[derive(Default)]
struct MemoryStore(Mutex<BTreeMap<String, Secret>>);

impl CredentialStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError> {
        self.0.lock().unwrap().insert(key.into(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn config() -> Config {
    Config {
        providers: BTreeMap::from([(
            "x".into(),
            ProviderProfileConfig {
                provider_type: ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: "https://example.test/v1".into(),
                credential: CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: "x".into(),
                },
                models: vec!["test-model".into()],
                excluded_models: vec![],
                default_model: "test-model".into(),
            },
        )]),
        ..Config::default()
    }
}

fn deps(store: Arc<dyn CredentialStore>) -> ComposeDeps {
    ComposeDeps {
        credential_store: store,
        event_bus: None,
        env: Arc::new(MapEnv::default()),
        catalog: model::ModelCatalog::builtin(),
        factory: routing::factory::FactoryOptions::default(),
    }
}

#[test]
fn compose_providers_resolves_keyring_secret_into_auth() {
    // Given: the GUI stores the secret under input.name, not service/account.
    let store = Arc::new(MemoryStore::default());
    store
        .set("x", &Secret::from("gui-secret".to_owned()))
        .unwrap();
    // When
    let composed = compose_providers(&config(), deps(store)).unwrap();
    // Then
    assert_eq!(
        composed.providers["x"].auth,
        providers::ProviderAuth::new("gui-secret")
    );
}

#[test]
fn compose_providers_fails_closed_when_keyring_secret_missing() {
    // Given
    let store = Arc::new(MemoryStore::default());
    // When
    let error = compose_providers(&config(), deps(store)).unwrap_err();
    // Then
    assert_eq!(
        error.to_string(),
        "keyring credential `x` for provider `x` is missing or empty"
    );
}

#[test]
fn compose_providers_fails_closed_when_keyring_secret_empty() {
    // Given
    let store = Arc::new(MemoryStore::default());
    store.set("x", &Secret::from("  ".to_owned())).unwrap();
    // When
    let error = compose_providers(&config(), deps(store)).unwrap_err();
    // Then
    assert_eq!(
        error.to_string(),
        "keyring credential `x` for provider `x` is missing or empty"
    );
}
