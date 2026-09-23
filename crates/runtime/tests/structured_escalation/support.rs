use std::{collections::BTreeMap, sync::Arc, time::Duration};

use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig,
};
use routing::{ComposeDeps, MapEnv, factory::FactoryOptions};
use runtime::AgentModel;
use runtime::compose::SwitchableModel;
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use serde_json::json;

use super::MODEL;

const KEY_ENV: &str = "EVORCH_STRUCTURED_REVIEW_KEY";

pub(super) struct Harness {
    _directory: tempfile::TempDir,
    pub(super) model: Arc<dyn AgentModel>,
}

pub(super) fn harness(base_url: &str, codex: bool, timeout: Duration) -> Harness {
    let directory = tempfile::tempdir().expect("test directory");
    let store = Arc::new(
        FileCredentialStore::open(directory.path().join("credentials")).expect("credential store"),
    );
    store
        .set(
            "test-account",
            &Secret::from(json!({
                "access_token":"access", "refresh_token":"refresh",
                "id_token":"e30.eyJleHAiOjQxMDI0NDQ4MDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2MifX0.signature"
            }).to_string()),
        )
        .expect("test tokens");
    let profile = ProviderProfileConfig {
        provider_type: if codex {
            ProviderTypeConfig::OpenAiCodex
        } else {
            ProviderTypeConfig::OpenAiCompatible
        },
        api_protocol: if codex {
            ApiProtocolConfig::OpenAiCodexResponses
        } else {
            ApiProtocolConfig::OpenAiCompletions
        },
        base_url: base_url.into(),
        credential: if codex {
            CredentialRefConfig::Keyring {
                service: "evorch".into(),
                account: "test-account".into(),
            }
        } else {
            CredentialRefConfig::Env {
                var: KEY_ENV.into(),
            }
        },
        models: vec![config::ModelEntryConfig::enabled(MODEL)],
        excluded_models: vec![],
        default_model: MODEL.into(),
    };
    let mut config = Config {
        providers: BTreeMap::from([("local".into(), profile)]),
        ..Config::default()
    };
    // Nondefault settings make accidental regeneration of the fallback request visible.
    config.agents.worker.base.generation.temperature = Some(0.25);
    config.agents.worker.base.generation.max_tokens = Some(321);
    let model = runtime::compose::compose_routed_model(
        &config,
        ComposeDeps {
            credential_store: store,
            event_bus: None,
            env: Arc::new(MapEnv::from_iter([(KEY_ENV, "test-key")])),
            catalog: model::ModelCatalog::new(),
            factory: FactoryOptions {
                auth_base_url_override: Some(base_url.into()),
                request_timeout: Some(timeout),
            },
        },
    )
    .expect("routed model");
    Harness {
        _directory: directory,
        // Production UI wraps routed models; structured requests must cross this seam.
        model: Arc::new(SwitchableModel::new(model)),
    }
}
