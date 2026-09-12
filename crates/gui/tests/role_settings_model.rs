use gui::model::role_settings::RoleSettingsModel;

#[test]
fn seed_preserves_all_bindings_when_config_has_overrides() {
    // Given: role and category overrides, including additional roles.
    let mut config = config::Config::default();
    config.agents.roles.oracle.preset = Some("oracle-preset".into());
    config.agents.worker.categories.insert(
        "quick".into(),
        config::CategoryBindingConfig {
            generation: config::GenerationOverridesConfig {
                temperature: Some(0.25),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    // When: seeding the editor.
    let editor = RoleSettingsModel::seed_from_config(&config);
    // Then: no override is lost or materialized.
    assert_eq!(editor.agents, config.agents);
    assert!(editor.validate().is_ok());
}

#[test]
fn picker_uses_enabled_provider_models_when_routes_are_automatic() {
    // Given: enabled and disabled provider models.
    let mut config = config::Config::default();
    let mut disabled = config::ModelEntryConfig::enabled("disabled");
    disabled.enabled = false;
    config.providers.insert(
        "local".into(),
        config::ProviderProfileConfig {
            models: vec![config::ModelEntryConfig::enabled("enabled"), disabled],
            ..Default::default()
        },
    );
    // When: seeding picker options.
    let editor = RoleSettingsModel::seed_from_config(&config);
    // Then: only enabled provider models appear.
    assert!(editor.logical_models.iter().any(|name| name == "enabled"));
    assert!(!editor.logical_models.iter().any(|name| name == "disabled"));
}
