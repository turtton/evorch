use config::{ModelEntryConfig, ProviderProfileConfig};

#[test]
fn codex_defaults_when_models_are_omitted_or_empty() {
    // Given
    for fields in ["", "models = []\ndefault_model = ''"] {
        let input = format!("type = 'openai-codex'\n{fields}");
        // When
        let profile: ProviderProfileConfig = toml::from_str(&input).unwrap();
        // Then
        assert_eq!(
            profile.models,
            [
                "gpt-6-astra",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5"
            ]
            .map(ModelEntryConfig::enabled)
        );
        assert_eq!(profile.default_model, "gpt-6-astra");
    }
}

#[test]
fn codex_preserves_explicit_models_and_default() {
    // Given
    let input =
        "type = 'openai-codex'\nmodels = ['custom-a', 'custom-b']\ndefault_model = 'custom-b'";
    // When
    let profile: ProviderProfileConfig = toml::from_str(input).unwrap();
    // Then
    assert_eq!(
        profile.models,
        ["custom-a", "custom-b"].map(ModelEntryConfig::enabled)
    );
    assert_eq!(profile.default_model, "custom-b");
}

#[test]
fn other_providers_keep_existing_defaults_when_models_are_omitted() {
    // Given
    for provider in [
        "anthropic",
        "anthropic-subscription",
        "openai",
        "github-copilot",
        "openrouter",
        "openai-compatible",
    ] {
        let input = format!("type = '{provider}'");
        // When
        let profile: ProviderProfileConfig = toml::from_str(&input).unwrap();
        // Then
        assert_eq!(
            profile.models,
            [ModelEntryConfig::enabled("claude-sonnet-4-5")]
        );
        assert_eq!(profile.default_model, "claude-sonnet-4-5");
    }
}

#[test]
fn kimi_subscription_defaults_when_fields_are_omitted() {
    // Given
    let profile: ProviderProfileConfig = toml::from_str("type = 'kimi-subscription'").unwrap();
    // Then
    assert_eq!(
        profile.provider_type,
        config::ProviderTypeConfig::KimiSubscription
    );
    assert_eq!(
        profile.base_url,
        config::types::provider::KIMI_DEFAULT_BASE_URL
    );
    assert_eq!(
        profile.api_protocol,
        config::ApiProtocolConfig::OpenAiCompletions
    );
    assert_eq!(
        profile.models,
        config::types::provider::KIMI_DEFAULT_MODELS
            .iter()
            .map(|id| ModelEntryConfig::enabled(*id))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        profile.default_model,
        config::types::provider::KIMI_DEFAULT_MODEL
    );
}
