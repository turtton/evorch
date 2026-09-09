use config::types::provider::ModelEntryConfig;
use config::{
    Config, ConfigError, LoadOptions, OpenAiCompatibleProviderInput, ProviderCredentialInput,
    save_openai_compatible_provider,
};

fn input() -> OpenAiCompatibleProviderInput {
    OpenAiCompatibleProviderInput {
        name: "local".into(),
        base_url: "https://example.com/v1".into(),
        credential: ProviderCredentialInput::Env {
            var: "API_KEY".into(),
        },
        models: vec![ModelEntryConfig::enabled("a"), disabled("b")],
        excluded_models: vec![],
        default_model: "a".into(),
    }
}

fn disabled(id: &str) -> ModelEntryConfig {
    ModelEntryConfig {
        id: id.into(),
        enabled: false,
    }
}

#[test]
fn save_writeback_enabled_as_string_disabled_as_inline_table() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    // When
    save_openai_compatible_provider(&path, &input()).unwrap();
    // Then
    let doc = std::fs::read_to_string(path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let models = doc["providers"]["local"]["models"].as_array().unwrap();
    assert_eq!(models.get(0).unwrap().as_str(), Some("a"));
    let table = models.get(1).unwrap().as_inline_table().unwrap();
    assert_eq!(table.get("id").unwrap().as_str(), Some("b"));
    assert_eq!(table.get("enabled").unwrap().as_bool(), Some(false));
}

#[test]
fn save_rejects_default_model_disabled() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let mut candidate = input();
    candidate.default_model = "b".into();
    // When
    let result = save_openai_compatible_provider(&tmp.path().join("evorch.toml"), &candidate);
    // Then
    assert!(matches!(result, Err(ConfigError::InvalidField { path, .. })
        if path == "providers.local.default_model"));
}

#[test]
fn save_rejects_all_models_disabled() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let mut candidate = input();
    candidate.models = vec![disabled("a")];
    // When
    let result = save_openai_compatible_provider(&tmp.path().join("evorch.toml"), &candidate);
    // Then
    assert!(matches!(result, Err(ConfigError::InvalidField { path, .. })
        if path == "providers.local.models"));
}

#[test]
fn save_legacy_string_models_still_valid() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    std::fs::write(
        &path,
        "version = 2\n[providers.other]\nmodels = ['legacy']\n",
    )
    .unwrap();
    // When
    save_openai_compatible_provider(&path, &input()).unwrap();
    // Then
    let config: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
        config.providers["other"].models,
        vec![ModelEntryConfig::enabled("legacy")]
    );
}

#[test]
fn reload_saved_config_preserves_enabled_flags() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let candidate = input();
    save_openai_compatible_provider(&tmp.path().join("evorch.toml"), &candidate).unwrap();
    // When
    let config = Config::load(&LoadOptions {
        project_dir: Some(tmp.path().to_path_buf()),
        user_config_dir: Some(tmp.path().join("empty")),
        read_env: false,
        ..LoadOptions::default()
    })
    .unwrap();
    // Then
    assert_eq!(config.providers["local"].models, candidate.models);
}

#[test]
fn save_normalizes_ids_and_prefers_enabled_duplicates_in_either_order() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    let mut candidate = input();
    candidate.models = vec![
        disabled(" a "),
        ModelEntryConfig::enabled("a"),
        ModelEntryConfig::enabled(" b "),
        disabled("b"),
        disabled(" c "),
        disabled("c"),
        ModelEntryConfig::enabled(" "),
    ];
    // When
    save_openai_compatible_provider(&path, &candidate).unwrap();
    // Then
    let config: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
        config.providers["local"].models,
        vec![
            ModelEntryConfig::enabled("a"),
            ModelEntryConfig::enabled("b"),
            disabled("c")
        ]
    );
}
