use super::*;
use providers::provider::claude::{CLAUDE_INSTALLATION_ID_FILE, ClaudeInstallationId};

#[test]
fn production_reload_reuses_the_user_config_installation_seed() {
    let directory = tempfile::tempdir().unwrap();
    let user_dir = directory.path().join("user-config");
    let mut config = config::Config::default();
    config.providers.insert(
        "claude".into(),
        config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::AnthropicSubscription,
            api_protocol: config::ApiProtocolConfig::AnthropicMessages,
            base_url: "https://example.test/v1".into(),
            credential: config::CredentialRefConfig::Keyring {
                service: "evorch".into(),
                account: "claude".into(),
            },
            models: vec![config::ModelEntryConfig::enabled("claude-test")],
            default_model: "claude-test".into(),
            ..Default::default()
        },
    );
    let context = ProductionModel {
        load_options: config::LoadOptions {
            user_config_dir: Some(user_dir.clone()),
            file_overrides: [(
                user_dir.join("config.toml"),
                toml::Value::try_from(&config).unwrap(),
            )]
            .into(),
            read_env: false,
            ..Default::default()
        },
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(directory.path().join("credentials"))
                .unwrap(),
        ),
        bus: Arc::new(event_bus::EventBus::new(16)),
        env: Arc::new(routing::MapEnv::default()),
    };
    context.reload().unwrap();
    let seed_path = user_dir.join(CLAUDE_INSTALLATION_ID_FILE);
    let seed = ClaudeInstallationId::load_or_create(&seed_path).unwrap();
    context.reload().unwrap();
    assert_eq!(
        ClaudeInstallationId::load_or_create(&seed_path).unwrap(),
        seed
    );
    std::fs::write(&seed_path, "private-corrupt-seed").unwrap();
    let error = context.reload().err().unwrap();
    assert!(!error.contains("private-corrupt-seed"));
    assert_eq!(
        std::fs::read_to_string(&seed_path).unwrap(),
        "private-corrupt-seed"
    );
}
