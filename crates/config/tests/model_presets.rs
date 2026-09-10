use config::{
    Config, LoadOptions, MetadataSource, ModelEntryConfig, ModelPresetConfig, ProviderProfileConfig,
};

#[test]
fn model_presets_table_parses() {
    let config: Config =
        toml::from_str("[model_presets.deepseek-v4]\ncontext_window = 128000").unwrap();
    assert_eq!(
        config.model_presets["deepseek-v4"],
        ModelPresetConfig {
            context_window: Some(128_000),
            ..ModelPresetConfig::default()
        }
    );
}

#[test]
fn model_entry_new_fields_optional() {
    let profile: ProviderProfileConfig =
        toml::from_str("models = [{ id = 'x', enabled = true }]").unwrap();
    assert_eq!(profile.models, [ModelEntryConfig::enabled("x")]);
    let entry: ModelEntryConfig = toml::from_str("id = 'x'").unwrap();
    assert!(entry.enabled);
}

#[test]
fn model_entry_metadata_source_parses() {
    for (source, expected) in [
        ("manual", MetadataSource::Manual),
        ("models-dev", MetadataSource::ModelsDev),
        ("provider-default", MetadataSource::ProviderDefault),
    ] {
        let entry: ModelEntryConfig =
            toml::from_str(&format!("id = 'x'\nmetadata_source = '{source}'")).unwrap();
        assert_eq!(entry.metadata_source, Some(expected));
        let value = toml::Value::try_from(entry).unwrap();
        assert_eq!(value["metadata_source"].as_str(), Some(source));
    }
}

#[test]
fn legacy_string_model_list_still_works() {
    let profile: ProviderProfileConfig = toml::from_str("models = ['gpt-4o', ' x ']").unwrap();
    assert_eq!(
        profile.models,
        [
            ModelEntryConfig::enabled("gpt-4o"),
            ModelEntryConfig::enabled("x")
        ]
    );
}

#[test]
fn model_entry_context_window_override_parses() {
    let entry: ModelEntryConfig = toml::from_str("id = 'x'\ncontext_window = 64000").unwrap();
    assert_eq!(entry.context_window, Some(64_000));
}

#[test]
fn model_entry_none_fields_are_omitted_on_roundtrip() {
    let entry = ModelEntryConfig::enabled("x");
    let serialized = toml::to_string(&entry).unwrap();
    let value: toml::Table = toml::from_str(&serialized).unwrap();
    assert_eq!(value.len(), 2);
    assert_eq!(
        toml::from_str::<ModelEntryConfig>(&serialized).unwrap(),
        entry
    );
}

#[test]
fn populated_model_metadata_roundtrips_through_real_loader() {
    let tmp = tempfile::tempdir().unwrap();
    let doc = "version = 2\n[providers.local]\nmodels = [{ id = 'x', enabled = true, metadata_source = 'models-dev', metadata_ref = 'deepseek/x', preset = 'deepseek-v4', context_window = 64000 }]\n[model_presets.deepseek-v4]\ncontext_window = 128000\nmax_output_tokens = 8192\ninput_price_per_million_usd = 0.25\noutput_price_per_million_usd = 1.5\n";
    std::fs::write(tmp.path().join("evorch.toml"), doc).unwrap();
    let config = Config::load(&LoadOptions {
        project_dir: Some(tmp.path().to_owned()),
        user_config_dir: Some(tmp.path().join("empty-user")),
        read_env: false,
        ..LoadOptions::default()
    })
    .unwrap();
    let expected: Config = toml::from_str(doc).unwrap();
    assert_eq!(
        config.providers["local"].models[0].metadata_ref.as_deref(),
        Some("deepseek/x")
    );
    assert_eq!(
        config.providers["local"].models[0].preset.as_deref(),
        Some("deepseek-v4")
    );
    assert_eq!(
        config.model_presets["deepseek-v4"],
        ModelPresetConfig {
            context_window: Some(128_000),
            max_output_tokens: Some(8192),
            input_price_per_million_usd: Some(0.25),
            output_price_per_million_usd: Some(1.5),
        }
    );
    assert_eq!(config, expected);
    let serialized = toml::to_string(&config).unwrap();
    assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), expected);
}

#[test]
fn empty_model_preset_fields_are_omitted_on_roundtrip() {
    let preset = ModelPresetConfig::default();
    let serialized = toml::to_string(&preset).unwrap();
    assert!(serialized.is_empty());
    assert_eq!(
        toml::from_str::<ModelPresetConfig>(&serialized).unwrap(),
        preset
    );
}

#[test]
fn invalid_model_metadata_is_rejected() {
    for doc in [
        "id = 'x'\nmetadata_source = 'unknown'",
        "id = 'x'\ncontext_window = -1",
        "id = 'x'\nmetadata_ref = 1",
        "id = 'x'\npreset = 1",
    ] {
        assert!(toml::from_str::<ModelEntryConfig>(doc).is_err());
    }
}
