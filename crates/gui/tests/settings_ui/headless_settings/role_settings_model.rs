use gui::model::role_settings::RoleSettingsModel;

#[test]
fn picker_uses_route_keys_and_explicit_bindings_only() {
    // Given: provider profile with enabled/disabled concrete model ids and one explicit binding.
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
    config.agents.roles.web_researcher.models =
        vec![config::ChainEntryConfig::route("explicit-binding")];
    config.routing.routes.insert(
        "declared-route".into(),
        vec![config::RouteCandidateConfig {
            profile: "local".into(),
            model: None,
            auto: false,
        }],
    );
    // When: seeding picker options.
    let editor = RoleSettingsModel::seed_from_config(&config);
    // Then: options come from route keys plus explicit bindings, never raw provider model ids.
    assert!(
        editor
            .logical_models
            .iter()
            .any(|name| name == "declared-route")
    );
    assert!(
        editor
            .logical_models
            .iter()
            .any(|name| name == "explicit-binding")
    );
    assert!(!editor.logical_models.iter().any(|name| name == "enabled"));
    assert!(!editor.logical_models.iter().any(|name| name == "disabled"));
}
