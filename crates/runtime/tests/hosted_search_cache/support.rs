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
                edit_tool: None,
            },
        )]),
        routing: config::RoutingConfig {
            cooldown: Default::default(),
            fallback: Default::default(),
            routes: BTreeMap::from([(
                "web_researcher".into(),
                vec![config::RouteCandidateConfig {
                    profile: "codex".into(),
                    model: Some(MODEL.into()),
                    auto: false,
                }],
            )]),
        },
        agents: config::AgentsConfig {
            roles: config::types::agents::AdditionalRoleBindings {
                web_researcher: config::RoleBindingConfig {
                    models: vec![config::ChainEntryConfig::with_effort(
                        "web_researcher",
                        "low",
                    )],
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
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
                ..Default::default()
            },
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap();
    (
        Arc::new(runtime::compose::SwitchableModel::new(model)),
        directory,
    )
}
