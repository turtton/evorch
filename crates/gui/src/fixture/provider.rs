/// Shared provider profiles for demo captures and headless layout tests.
pub fn demo_provider_config() -> config::Config {
    let mut config = config::Config::default();
    config.providers.insert(
        "local".into(),
        config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCompatible,
            base_url: "https://api.example.invalid/v1".into(),
            credential: config::CredentialRefConfig::Env {
                var: "EXAMPLE_API_KEY".into(),
            },
            models: vec![
                config::types::provider::ModelEntryConfig::enabled("gpt-4.1"),
                config::types::provider::ModelEntryConfig::enabled("gpt-4.1-mini"),
            ],
            default_model: "gpt-4.1".into(),
            ..Default::default()
        },
    );
    config.providers.insert(
        "work-codex".into(),
        config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCodex,
            models: config::types::provider::CODEX_DEFAULT_MODELS
                .iter()
                .copied()
                .map(config::ModelEntryConfig::enabled)
                .collect(),
            default_model: config::types::provider::CODEX_DEFAULT_MODEL.into(),
            ..Default::default()
        },
    );
    config
}
