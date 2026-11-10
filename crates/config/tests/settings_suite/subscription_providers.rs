use config::{ApiProtocolConfig, ModelEntryConfig, ProviderTypeConfig, SubscriptionProviderInput};

#[test]
fn provider_types_choose_their_native_protocol_and_preserve_secret_free_references() {
    for (kind, protocol) in [
        ("cursor", ApiProtocolConfig::CursorAgent),
        (
            "anthropic-subscription",
            ApiProtocolConfig::AnthropicMessages,
        ),
        ("anthropic", ApiProtocolConfig::AnthropicMessages),
    ] {
        let text = format!(
            "type = '{kind}'\ncredential = {{ type = 'keyring', service = 'evorch', account = 'account' }}"
        );
        let profile: config::ProviderProfileConfig = toml::from_str(&text).unwrap();
        assert_eq!(profile.api_protocol, protocol);
        assert!(!profile.models.is_empty());
        assert!(profile.models.iter().any(|m| m.id == profile.default_model));
        assert!(
            toml::from_str::<config::ProviderProfileConfig>(&format!(
                "{text}\naccess_token='secret'"
            ))
            .is_err()
        );
    }
}

#[test]
fn subscription_save_renames_atomically_and_rejects_invalid_input_without_touching_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let mut input = SubscriptionProviderInput {
        name: "claude".into(),
        provider_type: ProviderTypeConfig::AnthropicSubscription,
        account: "claude-account".into(),
        base_url: "https://api.anthropic.com/v1".into(),
        models: vec![ModelEntryConfig::enabled("claude-sonnet-4-6")],
        excluded_models: vec!["claude-opus-4-6".into()],
        default_model: "claude-sonnet-4-6".into(),
    };
    config::save_subscription_provider_edit(&path, &input, None).unwrap();
    input.name = "claude-work".into();
    config::save_subscription_provider_edit(&path, &input, Some("claude")).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    let value: toml::Value = toml::from_str(&saved).unwrap();
    assert!(value["providers"].get("claude").is_none());
    assert_eq!(
        value["providers"]["claude-work"]["credential"]["account"].as_str(),
        Some("claude-account")
    );
    assert_eq!(
        value["providers"]["claude-work"]["excluded_models"][0].as_str(),
        Some("claude-opus-4-6")
    );
    input.default_model = "missing".into();
    assert!(config::save_subscription_provider_edit(&path, &input, Some("claude-work")).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), saved);
}
