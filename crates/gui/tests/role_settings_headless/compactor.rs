use super::support::{finish, fixture};

fn saved(root: &std::path::Path) -> config::RoleBindingConfig {
    config::Config::load(&config::LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .expect("saved config")
    .agents
    .roles
    .compactor
}

#[test]
fn inherited_compactor_keeps_its_empty_chain_and_dormant_overrides() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    let binding = &mut harness
        .state_mut()
        .role_settings_mut()
        .agents
        .roles
        .compactor;
    binding.preset = Some("kept-preset".into());
    binding.generation.temperature = Some(0.75);
    let expected = binding.clone();

    harness.run();
    harness.click_label("Compactor (fallback)");
    harness.run();
    assert_eq!(
        harness.label_value("Compactor route").as_deref(),
        Some("Use running role's chain")
    );
    assert!(!harness.has_label("未定義 (route なし)"));
    assert!(!harness.has_label("route を作成"));
    assert!(!harness.has_label("Preset reference"));
    assert!(!harness.has_label("Category overrides"));
    // Generation settings have no effect while the running role's chain is inherited.
    harness.click_label("Generation overrides");
    harness.run();
    assert!(!harness.has_label("Temperature"));
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);

    assert_eq!(harness.state().role_settings().error, None);
    assert_eq!(saved(temp.path()), expected);
    harness.state_mut().open_role_settings();
    assert_eq!(
        harness.state().role_settings().agents.roles.compactor,
        expected
    );
}

#[test]
fn compactor_controls_save_chain_effort_generation_and_restore_inheritance() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    harness.run();
    harness.click_label("Compactor (fallback)");
    harness.run();
    harness.click_label("Compactor route");
    harness.run();
    harness.click_label("fast");
    harness.run();
    harness.click_label("Compactor route reasoning effort");
    harness.run();
    harness.click_label("high");
    harness.run();
    harness.click_label("Generation overrides");
    harness.run();
    for label in ["Temperature", "Top p", "Max tokens"] {
        harness.scroll_label_into_view(label);
        harness.run();
        harness.click_label(label);
        harness.run();
    }
    let mut expected = config::RoleBindingConfig {
        models: vec![config::ChainEntryConfig::with_effort("fast", "high")],
        generation: config::GenerationOverridesConfig {
            temperature: Some(0.0),
            top_p: Some(0.0),
            max_tokens: Some(1),
        },
        ..Default::default()
    };
    assert_eq!(
        harness.state().role_settings().agents.roles.compactor,
        expected
    );
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);
    assert_eq!(harness.state().role_settings().error, None);
    assert_eq!(saved(temp.path()), expected);
    harness.state_mut().open_role_settings();
    assert_eq!(
        harness.state().role_settings().agents.roles.compactor,
        expected
    );

    harness.run();
    harness.scroll_label_into_view("Compactor route");
    harness.run();
    harness.click_label("Compactor route");
    harness.run();
    harness.click_label("Use running role's chain");
    harness.run();
    expected.models.clear();
    assert!(!harness.has_label("未定義 (route なし)"));
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);
    assert_eq!(harness.state().role_settings().error, None);
    assert_eq!(saved(temp.path()), expected);
}

#[test]
fn unknown_compactor_route_opens_prefilled_routing_settings() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .roles
        .compactor
        .models = vec![config::ChainEntryConfig::route("summary-route")];
    harness.run();
    harness.click_label("Compactor (fallback)");
    harness.run();
    assert!(harness.has_label("未定義 (route なし)"));
    harness.click_label("route を作成");
    harness.run();

    assert!(!harness.state().role_settings().open);
    assert!(harness.state().routing_settings().open);
    assert_eq!(
        harness
            .state()
            .routing_settings()
            .pending_new_route
            .as_deref(),
        Some("summary-route")
    );
    assert!(
        harness
            .state()
            .routing_settings()
            .routes
            .contains_key("summary-route")
    );
}

#[test]
fn invalid_compactor_settings_are_rejected_before_saving() {
    for invalid in [
        config::RoleBindingConfig {
            models: vec![config::ChainEntryConfig::route("  ")],
            ..Default::default()
        },
        config::RoleBindingConfig {
            generation: config::GenerationOverridesConfig {
                max_tokens: Some(0),
                ..Default::default()
            },
            ..Default::default()
        },
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut harness, _) = fixture(temp.path());
        let path = config::project_main_config_path(temp.path());
        let before = std::fs::read(&path).expect("config");
        harness
            .state_mut()
            .role_settings_mut()
            .agents
            .roles
            .compactor = invalid;
        harness.run();
        harness.click_label("Save role settings");
        harness.run();

        let settings = harness.state().role_settings();
        let error = settings.error.as_deref().expect("validation error");
        assert!(error.contains("agents.roles.compactor"));
        assert!(harness.has_label(error));
        assert!(settings.open);
        assert!(!settings.is_saving());
        assert_eq!(std::fs::read(&path).expect("config"), before);
    }
}
