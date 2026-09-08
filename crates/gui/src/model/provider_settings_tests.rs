use super::*;
use config::{Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig};

#[test]
fn seed_from_config_selects_codex_tab_when_only_codex_profile() {
    // Given
    let mut config = Config::default();
    config.providers.insert("codex".into(), ProviderProfileConfig { provider_type: ProviderTypeConfig::OpenAiCodex, ..Default::default() });
    // When
    let model = ProviderSettingsModel::seed_from_config(&config);
    // Then
    assert_eq!(model.tab, ProviderSettingsTab::Codex);
}

#[test]
fn to_input_maps_keyring_mode_to_service_evorch_account_name() {
    // Given
    let model = ProviderSettingsModel::default();
    // When
    let input = model.to_input();
    // Then
    assert_eq!(input.credential, config::ProviderCredentialInput::Keyring { service: "evorch".into(), account: "openai-compat".into() });
}

fn compatible(credential: CredentialRefConfig) -> ProviderProfileConfig {
    ProviderProfileConfig {
        provider_type: ProviderTypeConfig::OpenAiCompatible,
        api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
        base_url: "https://example.com/v1".into(),
        credential,
        models: vec!["model-b".into(), "model-a".into()],
        default_model: "model-a".into(),
        excluded_models: vec!["excluded-b".into(), "excluded-a".into()],
    }
}

#[test]
fn seed_from_config_picks_first_openai_compatible_env_provider() {
    // Given
    let mut config = Config::default();
    config
        .providers
        .insert("a-anthropic".into(), ProviderProfileConfig::default());
    config.providers.insert(
        "z-compatible".into(),
        compatible(CredentialRefConfig::Env {
            var: "LATER_KEY".into(),
        }),
    );
    config.providers.insert(
        "b-compatible".into(),
        compatible(CredentialRefConfig::Env {
            var: "FIRST_KEY".into(),
        }),
    );
    // When
    let model = ProviderSettingsModel::seed_from_config(&config);
    // Then
    assert_eq!(
        model,
        ProviderSettingsModel {
            open: false,
            name: "b-compatible".into(),
            base_url: "https://example.com/v1".into(),
            api_key_env: "FIRST_KEY".into(),
            models_text: "model-b\nmodel-a".into(),
            default_model: "model-a".into(),
            error: None,
            excluded_models_text: "excluded-b\nexcluded-a".into(),
            available_models: None,
            models_fetch_state: ModelsFetchState::Idle,
            models_rx: None,
            models_fetch_base_url: None,
        }
    );
}

#[test]
fn seed_from_config_with_keyring_credential_leaves_api_key_env_empty() {
    // Given
    let mut config = Config::default();
    config.providers.insert(
        "keyring".into(),
        compatible(CredentialRefConfig::Keyring {
            service: "service".into(),
            account: "account".into(),
        }),
    );
    // When
    let model = ProviderSettingsModel::seed_from_config(&config);
    // Then
    assert_eq!(model.name, "keyring");
    assert_eq!(model.api_key_env, "");
    assert_eq!(model.base_url, "https://example.com/v1");
    assert_eq!(model.models_text, "model-b\nmodel-a");
    assert_eq!(model.default_model, "model-a");
    assert_eq!(model.excluded_models_text, "excluded-b\nexcluded-a");
}

#[test]
fn seed_from_config_without_openai_compatible_returns_default() {
    // Given
    let mut config = Config::default();
    config
        .providers
        .insert("anthropic".into(), ProviderProfileConfig::default());
    // When
    let model = ProviderSettingsModel::seed_from_config(&config);
    // Then
    assert_eq!(model, ProviderSettingsModel::default());
    assert_eq!(
        model,
        ProviderSettingsModel {
            open: false,
            name: "openai-compat".into(),
            base_url: String::new(),
            api_key_env: String::new(),
            models_text: String::new(),
            default_model: String::new(),
            error: None,
            excluded_models_text: String::new(),
            available_models: None,
            models_fetch_state: ModelsFetchState::Idle,
            models_rx: None,
            models_fetch_base_url: None,
        }
    );
}

#[test]
fn parsed_models_splits_trims_and_dedupes() {
    // Given
    let model = ProviderSettingsModel {
        models_text: " model-b, model-a\n\nmodel-b, , model-c\r\n model-a,\n".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let models = model.parsed_models();
    // Then
    assert_eq!(models, ["model-b", "model-a", "model-c"]);
}

#[test]
fn parsed_excluded_models_splits_trims_and_dedupes() {
    // Given
    let model = ProviderSettingsModel {
        excluded_models_text: " ex-b, ex-a\n\nex-b, , ex-c\r\n ex-a,\n".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let excluded = model.parsed_excluded_models();
    // Then
    assert_eq!(excluded, ["ex-b", "ex-a", "ex-c"]);
}

#[test]
fn to_input_uses_parsed_models_and_raw_fields() {
    // Given
    let model = ProviderSettingsModel {
        name: " raw-name ".into(),
        base_url: " https://example.com/v1 ".into(),
        api_key_env: " API_KEY ".into(),
        default_model: " model-b ".into(),
        models_text: " model-b,model-a\nmodel-b ".into(),
        excluded_models_text: " ex-a, ex-b\nex-a ".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let input = model.to_input();
    // Then
    assert_eq!(input.name, " raw-name ");
    assert_eq!(input.base_url, " https://example.com/v1 ");
    assert_eq!(input.api_key_env, " API_KEY ");
    assert_eq!(input.default_model, " model-b ");
    assert_eq!(input.models, ["model-b", "model-a"]);
    assert_eq!(input.excluded_models, ["ex-a", "ex-b"]);
}

#[test]
fn seed_from_config_round_trips_excluded_models() {
    // Given
    let mut config = Config::default();
    config.providers.insert(
        "roundtrip".into(),
        compatible(CredentialRefConfig::Env { var: "KEY".into() }),
    );
    // When
    let model = ProviderSettingsModel::seed_from_config(&config);
    let input = model.to_input();
    // Then
    assert_eq!(input.excluded_models, ["excluded-b", "excluded-a"]);
}

#[test]
fn poll_models_transitions_to_loaded_on_success() {
    // Given
    let (tx, rx) = channel();
    let mut model = ProviderSettingsModel {
        models_rx: Some(rx),
        models_fetch_state: ModelsFetchState::Loading,
        models_fetch_base_url: Some(String::new()),
        ..ProviderSettingsModel::default()
    };
    tx.send(Ok(vec!["fetched-a".into(), "fetched-b".into()]))
        .unwrap();
    // When
    let changed = model.poll_models();
    // Then
    assert!(changed);
    assert_eq!(model.models_fetch_state, ModelsFetchState::Loaded);
    assert_eq!(
        model.available_models,
        Some(vec!["fetched-a".into(), "fetched-b".into()])
    );
    assert!(model.models_rx.is_none());
}

#[test]
fn poll_models_transitions_to_failed_on_error() {
    // Given
    let (tx, rx) = channel();
    let mut model = ProviderSettingsModel {
        models_rx: Some(rx),
        models_fetch_state: ModelsFetchState::Loading,
        models_fetch_base_url: Some(String::new()),
        ..ProviderSettingsModel::default()
    };
    tx.send(Err("network error".into())).unwrap();
    // When
    let changed = model.poll_models();
    // Then
    assert!(changed);
    assert_eq!(
        model.models_fetch_state,
        ModelsFetchState::Failed("network error".into())
    );
    assert_eq!(model.available_models, None);
    assert!(model.models_rx.is_none());
}

#[test]
fn poll_models_returns_false_when_channel_is_empty() {
    // Given
    let (_tx, rx) = channel();
    let mut model = ProviderSettingsModel {
        models_rx: Some(rx),
        models_fetch_state: ModelsFetchState::Loading,
        ..ProviderSettingsModel::default()
    };
    // When
    let changed = model.poll_models();
    // Then
    assert!(!changed);
    assert_eq!(model.models_fetch_state, ModelsFetchState::Loading);
    assert!(model.models_rx.is_some());
}

#[test]
fn provider_status_of_empty_providers_is_not_configured() {
    // Given
    let config = Config::default();
    // When
    let status = provider_status_of(&config);
    // Then
    assert_eq!(
        status,
        ProviderStatus::NotConfigured {
            guidance: PROVIDER_MISSING_GUIDANCE.into()
        }
    );
}

#[test]
fn provider_status_of_non_empty_providers_is_configured() {
    // Given
    let mut config = Config::default();
    config
        .providers
        .insert("anthropic".into(), ProviderProfileConfig::default());
    // When
    let status = provider_status_of(&config);
    // Then
    assert_eq!(status, ProviderStatus::Configured);
}
