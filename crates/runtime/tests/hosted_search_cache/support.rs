use std::{collections::BTreeMap, sync::Arc};

use event_bus::EventBus;
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

pub(super) async fn model(
    server: &MockServer,
    bus: Arc<EventBus>,
) -> (Arc<dyn runtime::AgentModel>, tempfile::TempDir) {
    const MODEL: &str = "gpt-6-astra";
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{"slug":MODEL}]})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"tag_name":"rust-v0.156.1", "draft":false, "prerelease":false}
        ])))
        .mount(server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    store.set("offline-account", &Secret::from(json!({
        "access_token":"offline-access", "refresh_token":"offline-refresh",
        "id_token":"e30.eyJleHAiOjQwMDAwMDAwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NvdW50In19.sig"
    }).to_string())).unwrap();
    let config = config::Config {
        providers: BTreeMap::from([(
            "codex".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCodex,
                api_protocol: config::ApiProtocolConfig::OpenAiCodexResponses,
                base_url: format!("{}/backend-api/codex", server.uri()),
                credential: config::CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: "offline-account".into(),
                },
                models: vec![config::ModelEntryConfig::enabled(MODEL)],
                excluded_models: vec![],
                default_model: MODEL.into(),
            },
        )]),
        routing: config::RoutingConfig {
            routes: BTreeMap::from([(
                "web_researcher".into(),
                vec![config::RouteCandidateConfig {
                    profile: "codex".into(),
                    model: Some(MODEL.into()),
                    reasoning_effort: Some("low".into()),
                }],
            )]),
        },
        ..Default::default()
    };
    let model = runtime::compose::compose_routed_model(
        &config,
        routing::ComposeDeps {
            credential_store: store,
            event_bus: Some(bus),
            env: Arc::new(routing::MapEnv::default()),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions {
                auth_base_url_override: Some(server.uri()),
                codex_client_version: Some(providers::CodexClientVersion::Fixed(
                    providers::CodexCatalogVersion {
                        version: providers::CODEX_MODELS_FALLBACK_VERSION.into(),
                        warning: None,
                    },
                )),
                ..Default::default()
            },
        },
    )
    .unwrap()
    .with_codex_version_resolver(Arc::new(providers::CodexCatalogVersionResolver::new(
        format!("{}/releases", server.uri()),
    )));
    (
        Arc::new(runtime::compose::SwitchableModel::new(model)),
        directory,
    )
}
