use super::{finish, fixture};
use gui::{fixture::DemoSource, headless::HeadlessWorkbench};

#[test]
fn routing_prefill_save_returns_to_fresh_role_settings() {
    // Given: a persisted undefined role binding.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, runtime) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let config = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!("{config}\n[agents.explorer]\nlogical_model = 'legacy'\n"),
    )
    .expect("binding");
    state.open_role_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    harness.click_label("Explorer");
    harness.run();
    assert!(harness.has_label("未定義 (route なし)"));
    // When: create the missing route through the role modal and save its candidate.
    harness.click_label("route を作成");
    harness.run();
    enter_model(&mut harness, "legacy", "fast");
    harness.click_label("Save routing");
    harness.step();
    finish(&mut harness);
    // Then: return to a fresh role draft with a resolved preview and no warning.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    assert!(harness.state().role_settings().open);
    assert!(!harness.state().routing_settings().open);
    assert_eq!(
        harness
            .state()
            .role_settings()
            .agents
            .explorer
            .logical_model
            .as_deref(),
        Some("legacy")
    );
    assert!(!harness.has_label("未定義 (route なし)"));
    assert!(harness.has_label("→ accelerated/fast"));
    assert_eq!(
        runtime::AgentModel::selected_model(runtime.as_ref(), runtime::Role::Explorer, None),
        "accelerated/fast"
    );
}

#[test]
fn routing_registered_routes_start_collapsed() {
    // Given: a registered route loaded from disk.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let config = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!("{config}\n[routing.routes]\nregistered = [{{profile = 'local'}}]\n"),
    )
    .expect("route");
    state.open_routing_settings();
    // When: render the modal.
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // Then: the route name remains visible but its candidate editor is absent.
    assert!(harness.has_label("registered"));
    assert!(!harness.has_label("registered candidate 1 custom model ID"));
    assert!(!harness.has_label("Add candidate"));
}

#[test]
fn routing_expanded_route_can_edit_save_and_stays_expanded() {
    // Given: a registered route.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let config = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!("{config}\n[routing.routes]\nregistered = [{{profile = 'local'}}]\n"),
    )
    .expect("route");
    state.open_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // When: expand, edit and save through the real controls.
    harness.click_label("registered");
    harness.run();
    enter_model(&mut harness, "registered", "custom");
    harness.click_label("Save routing");
    harness.step();
    finish(&mut harness);
    // Then: the normal save remains in routing and preserves the expanded editor.
    assert!(harness.state().routing_settings().open);
    assert!(!harness.state().role_settings().open);
    assert_eq!(
        harness.state().routing_settings().routes["registered"][0]
            .model
            .as_deref(),
        Some("custom")
    );
    assert!(harness.has_label("registered candidate 1 custom model ID"));
}

fn model_fixture() -> HeadlessWorkbench<DemoSource> {
    let mut config = config::Config::default();
    for (name, provider_type, models) in [
        (
            "custom-a",
            config::ProviderTypeConfig::OpenAiCompatible,
            vec!["mA", "shared"],
        ),
        (
            "sandbox-sub",
            config::ProviderTypeConfig::AnthropicSubscription,
            vec!["mB", "shared"],
        ),
        (
            "z-codex",
            config::ProviderTypeConfig::OpenAiCodex,
            vec!["mB"],
        ),
        (
            "a-kimi",
            config::ProviderTypeConfig::KimiSubscription,
            vec!["mB"],
        ),
        ("api-z", config::ProviderTypeConfig::OpenAi, vec!["mZ"]),
    ] {
        config.providers.insert(
            name.into(),
            config::ProviderProfileConfig {
                provider_type,
                models: models
                    .into_iter()
                    .map(config::ModelEntryConfig::enabled)
                    .collect(),
                ..Default::default()
            },
        );
    }
    let mut state = gui::app::WorkbenchState::new(
        gui::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .expect("state");
    let mut model = gui::model::routing_settings::RoutingSettingsModel::seed_from_config_prefill(
        &config,
        "candidate",
    );
    model.routes.get_mut("candidate").expect("route")[0].profile = "custom-a".into();
    model.open = true;
    *state.routing_settings_mut() = model;
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn enter_model(harness: &mut HeadlessWorkbench<DemoSource>, route: &str, text: &str) {
    let label = format!("{route} candidate 1 custom model ID");
    harness.scroll_label_into_view(&label);
    harness.run();
    harness.click_label(&label);
    harness.run();
    harness
        .input_mut()
        .events
        .push(egui::Event::Text(text.into()));
    harness.run();
}

#[test]
fn routing_profiles_order_subscriptions_before_alphabetical_api_profiles() {
    // Given: a mix of all three subscription types and API profiles.
    let mut harness = model_fixture();
    // When: rendering the candidate editor.
    harness.run();
    // Then: the ordered list used by the dropdown groups subscriptions first.
    assert_eq!(
        harness.state().routing_settings().profile_names,
        ["a-kimi", "sandbox-sub", "z-codex", "api-z", "custom-a"]
    );
}

#[test]
fn routing_model_input_selects_first_matching_profile_in_both_directions() {
    for (profile, model, expected) in [
        ("custom-a", "mB", "a-kimi"),
        ("sandbox-sub", "mA", "custom-a"),
        ("sandbox-sub", "mB", "sandbox-sub"),
        ("custom-a", "shared", "custom-a"),
        ("sandbox-sub", "unknown", "sandbox-sub"),
    ] {
        // Given: a candidate with the requested current profile and empty override.
        let mut harness = model_fixture();
        harness
            .state_mut()
            .routing_settings_mut()
            .routes
            .get_mut("candidate")
            .expect("route")[0]
            .profile = profile.into();
        harness.run();
        // When: typing an exact model ID into the actual editor.
        enter_model(&mut harness, "candidate", model);
        // Then: keep compatible profiles or choose the first match without losing text.
        let candidate = &harness.state().routing_settings().routes["candidate"][0];
        assert_eq!(candidate.profile, expected, "{profile}/{model}");
        assert_eq!(candidate.model.as_deref(), Some(model));
    }
}
