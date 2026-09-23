#[path = "role_settings_headless/effort.rs"]
mod effort;
#[path = "role_settings_headless/evidence.rs"]
mod evidence;
#[path = "role_settings_headless/implicit.rs"]
mod implicit;
#[path = "role_settings_headless/legacy.rs"]
mod legacy;
#[path = "role_settings_headless/support.rs"]
mod support;
use support::{finish, fixture};

#[test]
fn logical_model_options_come_from_routing_routes() {
    // Given: explicit routes, provider IDs and legacy bindings.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    let mut config = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("config");
    config.agents.explorer.logical_model = Some("legacy".into());
    config.routing.routes.clear();
    for name in ["X", "Y"] {
        config
            .routing
            .routes
            .insert(name.into(), vec![config::RouteCandidateConfig::default()]);
    }
    // When: opening the seeded dropdown.
    let mut model = gui::model::role_settings::RoleSettingsModel::seed_from_config(&config);
    model.open = true;
    *harness.state_mut().role_settings_mut() = model;
    harness.run();
    harness.click_label("Explorer");
    harness.run();
    harness.click_label("Role logical model");
    harness.run();
    // Then: route keys and the legacy binding appear, not provider model IDs.
    assert_eq!(
        harness.state().role_settings().logical_models,
        ["X", "Y", "legacy"]
    );
    for label in ["X", "Y", "legacy"] {
        assert!(harness.has_label(label));
    }
    for label in ["base", "fast"] {
        assert!(!harness.has_label(label));
    }
}

#[test]
fn empty_routes_options_keep_only_explicit_bindings() {
    // Given: a provider config without routes and with a legacy category binding.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!(
            "{}\n[agents.worker.categories.quick]\nlogical_model = 'legacy'",
            text.split("[routing.routes]").next().expect("profiles")
        ),
    )
    .expect("write");
    // When: reopening from disk.
    harness.state_mut().open_role_settings();
    // Then: raw provider IDs and implicit role names are not offered.
    assert_eq!(harness.state().role_settings().logical_models, ["legacy"]);
}

#[test]
fn undefined_binding_shows_warning_and_route_create_button() {
    // Given: an unknown explicit binding.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .logical_model = Some("unknown".into());
    harness.run();
    // When: opening the binding's row.
    harness.click_label("Explorer");
    harness.run();
    // Then: the warning is actionable without blocking save.
    assert!(harness.has_label("未定義 (route なし)"));
    assert!(harness.has_label("route を作成"));
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);
    assert_eq!(harness.state().role_settings().error, None);
    assert!(!harness.state().role_settings().open);
    harness.state_mut().open_role_settings();
    assert_eq!(
        harness
            .state()
            .role_settings()
            .agents
            .explorer
            .logical_model
            .as_deref(),
        Some("unknown")
    );
}

#[test]
fn resolved_preview_shows_profile_model_per_role() {
    // Given: actual runtime bindings with distinct worker and category routes.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    let agents = &mut harness.state_mut().role_settings_mut().agents;
    agents.orchestrator.logical_model = Some("fast".into());
    agents.explorer.logical_model = Some("fast".into());
    agents.reviewer.logical_model = Some("fast".into());
    agents.roles.librarian.logical_model = Some("fast".into());
    agents.roles.planner.logical_model = Some("fast".into());
    agents.roles.oracle.logical_model = Some("fast".into());
    agents.roles.multimodal_looker.logical_model = Some("fast".into());
    agents.worker.categories.insert(
        "quick".into(),
        config::CategoryBindingConfig {
            logical_model: Some("fast".into()),
            ..Default::default()
        },
    );
    harness.state_mut().submit_role_settings();
    finish(&mut harness);
    // When: reopening each fixed role and the worker category.
    for role in [
        "Orchestrator",
        "Explorer",
        "Worker",
        "Reviewer",
        "Librarian",
        "Planner",
        "Oracle",
        "Multimodal Looker",
    ] {
        harness.state_mut().open_role_settings();
        harness.run();
        harness.click_label(role);
        harness.run();
        // Then: every row displays the runtime resolution, with categories only on worker.
        let expected = if role == "Worker" {
            "→ local/base"
        } else {
            "→ accelerated/fast"
        };
        assert!(harness.has_label(expected), "{role}");
        if role == "Worker" {
            harness.click_label("quick");
            harness.run();
            assert!(harness.has_label("→ accelerated/fast"));
        }
        harness.click_label(role);
        harness.run();
    }
}

#[test]
fn empty_routes_banner_in_role_settings_explains_missing_routes() {
    // Given: two profiles and no routes.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        text.split("[routing.routes]").next().expect("profiles"),
    )
    .expect("write");
    // When: opening settings from the changed config.
    harness.state_mut().open_role_settings();
    harness.run();
    // Then: roles without matching routes fail on use rather than resolving implicitly.
    assert!(harness.has_label(
        "No routes configured. Roles without a matching route fail when used — assign models that have routes, or create routes in Routing settings."
    ));
}

#[test]
fn route_create_from_role_settings_opens_prefilled_routing_modal() {
    // Given: a binding without a route.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .explorer
        .logical_model = Some("legacy".into());
    harness.run();
    harness.click_label("Explorer");
    harness.run();
    // When: using the actual cross-modal action.
    harness.click_label("route を作成");
    harness.run();
    // Then: only routing is open and the logical name is preserved in its new row.
    assert!(!harness.state().role_settings().open);
    assert!(!harness.has_label("Agent role settings"));
    assert!(harness.state().routing_settings().open);
    assert!(harness.has_label("Routing settings"));
    assert_eq!(
        harness
            .state()
            .routing_settings()
            .pending_new_route
            .as_deref(),
        Some("legacy")
    );
    assert_eq!(
        harness.state().routing_settings().routes["legacy"][0].profile,
        "accelerated"
    );
    assert!(harness.has_label("New route (not saved)"));
}
