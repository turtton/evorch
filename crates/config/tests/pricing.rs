use config::types::provider::{ModelEntryConfig, ModelPricing};
use config::{
    Config, OpenAiCompatibleProviderInput, ProviderCredentialInput, save_openai_compatible_provider,
};

#[test]
fn model_entry_cost_fields_roundtrip_via_save() {
    // Given: explicit per-million-token prices in a model definition.
    let model: ModelEntryConfig = toml::from_str(
        "id = 'y'\ninput_price = 1.25\noutput_price = 5.0\ncache_read_price = 0.125\ncache_write_price = 1.5",
    ).expect("model parses");
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("evorch.toml");
    let input = OpenAiCompatibleProviderInput {
        name: "x".into(),
        base_url: "https://example.com/v1".into(),
        credential: ProviderCredentialInput::Env {
            var: "API_KEY".into(),
        },
        models: vec![model.clone()],
        excluded_models: vec![],
        default_model: "y".into(),
    };
    // When: use the public save surface and deserialize its output.
    save_openai_compatible_provider(&path, &input).expect("save");
    let loaded: Config =
        toml::from_str(&std::fs::read_to_string(path).expect("read")).expect("load");
    // Then: every price survives, including enabled models usually saved as strings.
    assert_eq!(loaded.providers["x"].models[0], model);
    assert_eq!(
        model.pricing_for(None),
        Some(ModelPricing {
            input: Some(1.25),
            output: Some(5.0),
            cache_read: Some(0.125),
            cache_write: Some(1.5),
        })
    );
}

#[test]
fn static_toml_overrides_catalog_pricing() {
    // Given: static overrides differ from catalog prices; zero means free, not unknown.
    let model: ModelEntryConfig =
        toml::from_str("id = 'y'\ninput_price = 0.0\ncache_write_price = 7.0")
            .expect("model parses");
    let catalog = ModelPricing {
        input: Some(2.0),
        output: Some(8.0),
        cache_read: Some(0.2),
        cache_write: Some(3.0),
    };
    // When
    let resolved = model.pricing_for(Some(catalog));
    // Then: only absent static fields fall back.
    assert_eq!(
        resolved,
        Some(ModelPricing {
            input: Some(0.0),
            output: Some(8.0),
            cache_read: Some(0.2),
            cache_write: Some(7.0),
        })
    );
}

#[test]
fn unknown_pricing_returns_none() {
    // Given
    let model = ModelEntryConfig::enabled("unknown");
    // When
    let resolved = model.pricing_for(None);
    // Then
    assert_eq!(resolved, None);
}

#[test]
fn catalog_only_pricing_preserves_unknown_components() {
    // Given
    let model = ModelEntryConfig::enabled("y");
    let catalog = ModelPricing {
        output: Some(8.0),
        ..ModelPricing::default()
    };
    // When
    let resolved = model.pricing_for(Some(catalog));
    // Then
    assert_eq!(resolved, Some(catalog));
}

#[test]
fn empty_catalog_pricing_returns_none() {
    // Given
    let model = ModelEntryConfig::enabled("unknown");
    // When
    let resolved = model.pricing_for(Some(ModelPricing::default()));
    // Then
    assert_eq!(resolved, None);
}
