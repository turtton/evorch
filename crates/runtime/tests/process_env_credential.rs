use std::collections::BTreeMap;
use std::sync::Arc;

use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig,
};
use event_bus::EventBus;
use routing::ProcessEnv;
use runtime::{CompositionError, ModelSource, RuntimeComposition, compose_runtime};
use sandbox::DirectSandbox;
use sandbox::credential::{CredentialStore, FileCredentialStore};
use tools::ToolExecutor;

const KEY_ENV: &str = "EVORCH_TEST_KEY_PROCESS_ENV";

struct EnvGuard(Option<String>);

impl EnvGuard {
    fn set(value: &str) -> Self {
        let guard = Self(std::env::var(KEY_ENV).ok());
        // SAFETY: Category 13 — library contract. This dedicated test binary has one test,
        // so no other test thread can read or mutate KEY_ENV while this scope owns the guard.
        unsafe { std::env::set_var(KEY_ENV, value) };
        guard
    }

    fn remove() {
        // SAFETY: Category 13 — library contract. The single test owns KEY_ENV for the full
        // guard lifetime and performs no concurrent environment access.
        unsafe { std::env::remove_var(KEY_ENV) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: Category 13 — library contract. Drop runs before the single test releases
        // ownership of KEY_ENV, restoring exactly the value captured by EnvGuard::set.
        unsafe {
            match &self.0 {
                Some(value) => std::env::set_var(KEY_ENV, value),
                None => std::env::remove_var(KEY_ENV),
            }
        }
    }
}

fn config() -> Config {
    Config {
        providers: BTreeMap::from([(
            "local".to_string(),
            ProviderProfileConfig {
                provider_type: ProviderTypeConfig::OpenAiCompatible,
                api_protocol: ApiProtocolConfig::OpenAiCompletions,
                base_url: "http://127.0.0.1:1/v1".to_string(),
                credential: CredentialRefConfig::Env {
                    var: KEY_ENV.to_string(),
                },
                models: vec!["local-model".to_string()],
                default_model: "local-model".to_string(),
            },
        )]),
        ..Config::default()
    }
}

fn compose(config: &Config, root: &std::path::Path) -> Result<(), CompositionError> {
    let bus = Arc::new(EventBus::new(32));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let credential_store: Arc<dyn CredentialStore> =
        Arc::new(FileCredentialStore::open(root).expect("credential store"));
    compose_runtime(RuntimeComposition {
        config,
        bus,
        executor,
        credential_store,
        env: Arc::new(ProcessEnv),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .map(|_| ())
}

// Given: ProcessEnv が読む credential を設定し、元値を復元する guard
// When: 設定中と削除後に同じ configured composition を評価する
// Then: 設定中は成功し、削除後は MissingCredential で eager failure する
#[test]
fn process_env_resolves_and_then_rejects_removed_credential() {
    let guard = EnvGuard::set("process-env-key");
    let directory = tempfile::tempdir().expect("credential directory");
    let config = config();

    assert_eq!(compose(&config, directory.path()), Ok(()));
    EnvGuard::remove();
    let error = compose(&config, directory.path()).expect_err("removed key must fail");

    assert!(matches!(
        error,
        CompositionError::Routing(routing::RoutingError::MissingCredential { profile, var })
            if profile == "local" && var == KEY_ENV
    ));
    drop(guard);
}
