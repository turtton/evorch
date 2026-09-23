use super::{finish, fixture};
use gui::headless::HeadlessWorkbench;
use runtime::{AgentModel, Role};

#[test]
fn route_rename_saves_agents_and_reseeds_models() {
    // Given: a route referenced explicitly by explorer and implicitly by no role.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, runtime) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!(
            "{text}\n[routing.routes]\nold = [{{profile = 'local'}}]\n\
             [agents.explorer]\nlogical_model = 'old'\n"
        ),
    )
    .expect("route and binding");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .route_name_edits
        .insert("old".into(), "new".into());
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // When: saving the draft rename through the actual modal.
    harness.click_label("Save routing");
    harness.step();
    finish(&mut harness);
    // Then: disk, reseeded routing/role models and runtime use the new name.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    let saved = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("saved");
    assert!(saved.routing.routes.contains_key("new"));
    assert!(!saved.routing.routes.contains_key("old"));
    assert_eq!(saved.agents.explorer.logical_model.as_deref(), Some("new"));
    assert_eq!(saved.agents.worker.logical_model, None);
    assert_eq!(
        harness.state().routing_settings().routes,
        saved.routing.routes
    );
    assert_eq!(
        harness.state().routing_settings().route_users["new"],
        ["explorer"]
    );
    assert!(
        harness
            .state()
            .routing_settings()
            .route_name_edits
            .is_empty()
    );
    assert_eq!(runtime.selected_model(Role::Explorer, None), "local/base");
    harness.state_mut().open_role_settings();
    assert_eq!(harness.state().role_settings().agents, saved.agents);
}

#[test]
fn route_rename_loads_fresh_effective_agents_from_lower_layers() {
    // Given: routing is opened before a user-layer binding is added.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!("{text}\n[routing.routes]\nold = [{{profile = 'local'}}]\n"),
    )
    .expect("route");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .route_name_edits
        .insert("old".into(), "new".into());
    std::fs::create_dir_all(temp.path().join("user")).expect("user directory");
    let user_path = temp.path().join("user/config.toml");
    let user_config = "[agents.explorer]\nlogical_model = 'old'\npreset = 'keep-me'\n";
    std::fs::write(&user_path, user_config).expect("new lower-layer binding");
    // When: submitting after the lower-layer update.
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);
    // Then: the project shadows the old ref without changing the lower layer.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    let project = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("no-user-layer")),
        read_env: false,
        ..Default::default()
    })
    .expect("project-only config");
    assert_eq!(
        project.agents.explorer.logical_model.as_deref(),
        Some("new")
    );
    assert_eq!(project.agents.explorer.preset, None);
    assert_eq!(
        std::fs::read_to_string(user_path).expect("user config"),
        user_config
    );
    let effective = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("effective config");
    assert_eq!(effective.agents.explorer.preset.as_deref(), Some("keep-me"));
    harness.state_mut().open_role_settings();
    assert_eq!(harness.state().role_settings().agents, effective.agents);
}

#[test]
fn route_rename_blocked_by_project_dropin_does_not_write_or_reload_runtime() {
    // Given: the main file and a higher-priority drop-in both pin worker to old.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, runtime) = fixture(temp.path());
    let selected_before = runtime.selected_model(Role::Worker, None);
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    let original = format!(
        "{text}\n[routing.routes]\nold = [{{profile = 'local'}}]\n\
         [agents.worker]\nlogical_model = 'old'\n"
    );
    std::fs::write(&path, &original).expect("main");
    let dropins = temp.path().join("config.d");
    std::fs::create_dir_all(&dropins).expect("drop-in directory");
    let extra = "[agents.worker]\nlogical_model = 'old'\n";
    std::fs::write(dropins.join("extra.toml"), extra).expect("drop-in");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .rename_route("old", "new")
        .expect("rename");
    // When: submitting a rename that cannot override the drop-in binding.
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);
    // Then: the error identifies the binding and expected name, with no disk or runtime change.
    let error = harness
        .state()
        .routing_settings()
        .validation_error
        .as_deref()
        .expect("blocked");
    assert!(error.contains("Route rename blocked"), "{error}");
    assert!(error.contains("worker -> new"), "{error}");
    assert!(error.contains("No changes were saved"), "{error}");
    assert!(harness.has_label(error));
    assert_eq!(std::fs::read_to_string(&path).expect("main"), original);
    assert_eq!(
        std::fs::read_to_string(dropins.join("extra.toml")).expect("drop-in"),
        extra
    );
    let disk: config::Config = toml::from_str(&original).expect("disk config");
    assert!(disk.routing.routes.contains_key("old"));
    assert!(!disk.routing.routes.contains_key("new"));
    assert_eq!(disk.agents.worker.logical_model.as_deref(), Some("old"));
    assert_eq!(runtime.selected_model(Role::Worker, None), selected_before);
    assert_eq!(
        harness.state().routing_settings().route_renames(),
        [("old".into(), "new".into())].into()
    );
}

#[test]
fn route_rename_blocked_by_env_or_cli_preserves_all_rewritten_bindings() {
    // Given: higher-priority settings pin a named role and a worker category to old.
    for cli in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut state, runtime) = super::support::fixture_with_options(temp.path(), |options| {
            if cli {
                options.cli_overrides = Some(
                    toml::from_str(
                        "[agents.roles.planner]\nlogical_model = 'old'\n\
                     [agents.worker.categories.quick]\nlogical_model = 'old'\n",
                    )
                    .expect("CLI"),
                );
            } else {
                options.read_env = true;
                options.env = Some(
                    [
                        (
                            "EVORCH_AGENTS__ROLES__PLANNER__LOGICAL_MODEL".into(),
                            "old".into(),
                        ),
                        (
                            "EVORCH_AGENTS__WORKER__CATEGORIES__QUICK__LOGICAL_MODEL".into(),
                            "old".into(),
                        ),
                    ]
                    .into(),
                );
            }
        });
        let selected_before = runtime.selected_model(Role::Worker, Some("quick"));
        let path = temp.path().join("evorch.toml");
        let text = std::fs::read_to_string(&path).expect("config");
        let original = format!("{text}\n[routing.routes]\nold = [{{profile = 'local'}}]\n");
        std::fs::write(&path, &original).expect("route");
        state.open_routing_settings();
        state
            .routing_settings_mut()
            .rename_route("old", "new")
            .expect("rename");
        // When: submitting with injected env or CLI (without modifying process environment).
        state.submit_routing_settings();
        let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
        finish(&mut harness);
        // Then: both shadowed addresses and their intended names are reported without a save.
        let error = harness
            .state()
            .routing_settings()
            .validation_error
            .as_deref()
            .expect("blocked");
        assert!(error.contains("Route rename blocked"), "{error}");
        assert!(error.contains("roles.planner -> new"), "{error}");
        assert!(error.contains("worker.categories.quick -> new"), "{error}");
        assert_eq!(std::fs::read_to_string(path).expect("main"), original);
        assert_eq!(
            runtime.selected_model(Role::Worker, Some("quick")),
            selected_before
        );
    }
}

#[test]
fn route_rename_allows_unrelated_project_dropin_binding() {
    // Given: a drop-in overrides only reviewer, not the worker binding being renamed.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, runtime) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!(
            "{text}\n[routing.routes]\nold = [{{profile = 'local'}}]\n\
             review-route = [{{profile = 'accelerated'}}]\n\
             [agents.worker]\nlogical_model = 'old'\n"
        ),
    )
    .expect("main");
    let dropins = temp.path().join("config.d");
    std::fs::create_dir_all(&dropins).expect("drop-in directory");
    let extra = "[agents.reviewer]\nlogical_model = 'review-route'\n";
    std::fs::write(dropins.join("extra.toml"), extra).expect("drop-in");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .rename_route("old", "new")
        .expect("rename");
    // When: submitting and polling the real save worker.
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);
    // Then: the renamed binding works and the unrelated higher-layer value is preserved.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    let saved = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("saved");
    assert_eq!(saved.agents.worker.logical_model.as_deref(), Some("new"));
    assert_eq!(
        saved.agents.reviewer.logical_model.as_deref(),
        Some("review-route")
    );
    assert!(saved.routing.routes.contains_key("new"));
    assert!(!saved.routing.routes.contains_key("old"));
    assert_eq!(
        std::fs::read_to_string(dropins.join("extra.toml")).expect("drop-in"),
        extra
    );
    assert_eq!(runtime.selected_model(Role::Worker, None), "local/base");
    assert_eq!(
        runtime.selected_model(Role::Reviewer, None),
        "accelerated/fast"
    );
}

#[test]
fn route_rename_allows_old_route_retained_in_user_layer() {
    // Given: the user layer owns both the old route and an explicit binding.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    let user = temp.path().join("user");
    std::fs::create_dir_all(&user).expect("user directory");
    let original = "[routing.routes]\nold = [{profile = 'local'}]\n\
                    [agents.worker]\nlogical_model = 'old'\n";
    std::fs::write(user.join("config.toml"), original).expect("user config");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .rename_route("old", "new")
        .expect("rename");
    // When: saving the renamed route and binding into the project main file.
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);
    // Then: the lower-layer key remains visible without blocking the effective binding rewrite.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    assert!(
        harness
            .state()
            .routing_settings()
            .routes
            .contains_key("old")
    );
    assert!(
        harness
            .state()
            .routing_settings()
            .routes
            .contains_key("new")
    );
    assert_eq!(
        harness.state().routing_settings().route_users["new"],
        ["worker"]
    );
    assert!(harness.state().routing_settings().route_users["old"].is_empty());
    assert_eq!(
        std::fs::read_to_string(user.join("config.toml")).expect("user config"),
        original
    );
    let project: config::Config =
        toml::from_str(&std::fs::read_to_string(temp.path().join("evorch.toml")).expect("project"))
            .expect("project config");
    assert!(!project.routing.routes.contains_key("old"));
    assert!(project.routing.routes.contains_key("new"));
    assert_eq!(project.agents.worker.logical_model.as_deref(), Some("new"));
}

#[test]
fn route_rename_load_error_keeps_disk_untouched_and_shows_error() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .add_route("old")
        .expect("route");
    state
        .routing_settings_mut()
        .route_name_edits
        .insert("old".into(), "new".into());
    // Given: the effective config becomes invalid after opening the editor.
    std::fs::create_dir_all(temp.path().join("user")).expect("user directory");
    std::fs::write(temp.path().join("user/config.toml"), "[broken").expect("invalid config");
    let path = temp.path().join("evorch.toml");
    let before = std::fs::read(&path).expect("before");
    // When: saving a rename requires loading that effective config first.
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);
    // Then: the error is visible and the project has not been written.
    let error = harness
        .state()
        .routing_settings()
        .validation_error
        .as_deref()
        .expect("error");
    assert!(harness.has_label(error));
    assert_eq!(std::fs::read(path).expect("after"), before);
}

#[test]
fn identity_route_edits_do_not_materialize_agents() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .add_route("worker")
        .expect("route");
    state
        .routing_settings_mut()
        .route_name_edits
        .insert("worker".into(), "worker".into());
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);
    assert_eq!(harness.state().routing_settings().validation_error, None);
    let text = std::fs::read_to_string(temp.path().join("evorch.toml")).expect("config");
    assert!(!text.contains("[agents"));
}

#[test]
fn renaming_implicit_worker_route_leaves_an_actionable_missing_route() {
    // Given: worker inherits its role name, with a route under that name.
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = fixture(temp.path());
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!("{text}\n[routing.routes]\nworker = [{{profile = 'local'}}]\n"),
    )
    .expect("route");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .route_name_edits
        .insert("worker".into(), "w".into());
    // When: saving does not turn implicit refs into explicit refs.
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);
    assert_eq!(harness.state().routing_settings().validation_error, None);
    harness.state_mut().open_role_settings();
    harness.run();
    harness.click_label("Worker");
    harness.run();
    // Then: even with routes configured, the dangling implicit name is warned.
    assert!(!harness.state().role_settings().routes_empty);
    assert_eq!(
        harness.state().role_settings().agents.worker.logical_model,
        None
    );
    assert!(harness.has_label("未定義 (route なし)"));
    assert!(harness.has_label("route を作成"));
    harness.click_label("route を作成");
    harness.run();
    assert_eq!(
        harness
            .state()
            .routing_settings()
            .pending_new_route
            .as_deref(),
        Some("worker")
    );
    assert!(harness.state().routing_settings().routes.contains_key("w"));
}

#[test]
fn route_rename_does_not_persist_unrelated_cli_agent_override() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut state, _) = super::support::fixture_with_options(temp.path(), |options| {
        options.cli_overrides = Some(
            toml::from_str("[agents.reviewer]\nlogical_model = 'temporary'\n")
                .expect("CLI override"),
        );
    });
    let path = temp.path().join("evorch.toml");
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        format!(
            "{text}\n[routing.routes]\nold = [{{profile = 'local'}}]\n\
             [agents.worker]\nlogical_model = 'old'\n\
             [agents.reviewer]\nlogical_model = 'stable'\n"
        ),
    )
    .expect("project settings");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .rename_route("old", "new")
        .expect("rename");
    state.submit_routing_settings();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    finish(&mut harness);

    assert_eq!(harness.state().routing_settings().validation_error, None);
    let project = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("no-user-layer")),
        read_env: false,
        ..Default::default()
    })
    .expect("project config");
    assert_eq!(project.agents.worker.logical_model.as_deref(), Some("new"));
    assert_eq!(
        project.agents.reviewer.logical_model.as_deref(),
        Some("stable")
    );
}
