use std::collections::BTreeMap;

use gui::model::role_settings::{RoleSettingsModel, effort_options};

#[test]
fn librarian_unknown_binding_is_allowed_but_unrouted() {
    // Given: a librarian assignment discovered during editor seeding.
    let mut config = config::Config::default();
    config.agents.roles.librarian.logical_model = Some("research-model".into());
    let mut editor = RoleSettingsModel::seed_from_config(&config);
    assert!(
        editor
            .logical_models
            .iter()
            .any(|name| name == "research-model")
    );
    // When: the assignment is changed to a name that has no route yet.
    editor.agents.roles.librarian.logical_model = Some("unknown".into());
    // Then: validation accepts it (UI warns and offers route creation) while no route claims it.
    assert!(editor.validate().is_ok());
    assert!(!editor.route_names.contains("unknown"));
}

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
    config.agents.roles.librarian.logical_model = Some("explicit-binding".into());
    config.routing.routes.insert(
        "declared-route".into(),
        vec![config::RouteCandidateConfig {
            profile: "local".into(),
            model: None,
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

#[test]
fn effort_options_includes_max_and_preserves_model_override() {
    // Given: no model-specific options and a distinct model-specific override.
    let mut choices = BTreeMap::new();
    choices.insert("custom".into(), vec!["custom-level".into()]);
    // When: resolving options for an unregistered and a registered model.
    let defaults = effort_options(&choices, Some("default"));
    let override_options = effort_options(&choices, Some("custom"));
    // Then: max is the highest default option, while the override wins unchanged.
    assert_eq!(defaults.last().map(String::as_str), Some("max"));
    assert_eq!(override_options, vec!["custom-level"]);
}
