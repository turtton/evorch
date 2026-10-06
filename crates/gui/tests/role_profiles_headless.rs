//! Role profiles in the role/routing settings modals and the project settings dialog.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gui::model::project_dialog::ProjectDialog;
use gui::model::role_profiles::RoleProfileAction;
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use runtime::{AgentModel, Role, compose::SwitchableModel};
use workspace_ui::{ProjectId, SidebarState};

const USER_CONFIG: &str = r#"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["base"]
default_model = "base"
[providers.accelerated]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["fast"]
default_model = "fast"
[routing.routes]
worker = [{ profile = "local" }]
fast = [{ profile = "accelerated" }]
[role_profiles.fast.routing.routes]
worker = [{ profile = "accelerated" }]
"#;

fn user_options(root: &Path) -> config::LoadOptions {
    config::LoadOptions {
        user_config_dir: Some(root.join("user")),
        read_env: false,
        ..Default::default()
    }
}

/// A user layer with a `fast` profile and an active project `demo` at `<root>/project`.
fn fixture(
    root: &Path,
    project_profile: Option<&str>,
) -> (HeadlessWorkbench<DemoSource>, Arc<SwitchableModel>) {
    let user = root.join("user");
    let project = root.join("project");
    std::fs::create_dir_all(&user).expect("user directory");
    std::fs::create_dir_all(&project).expect("project directory");
    std::fs::write(user.join("config.toml"), USER_CONFIG).expect("user config");
    if project_profile.is_some() {
        config::save_project_role_profile(&project, project_profile).expect("project selection");
    }
    let context = gui::model::production::ProductionModel {
        load_options: config::LoadOptions {
            project_dir: Some(project.clone()),
            ..user_options(root)
        },
        credential_store: Arc::new(
            sandbox::FileCredentialStore::open(root.join("credentials")).expect("store"),
        ),
        bus: Arc::new(event_bus::EventBus::new(32)),
        env: Arc::new(routing::MapEnv::from_iter([("TEST_KEY", "test-secret")])),
    };
    let model = Arc::new(SwitchableModel::new(context.reload().expect("runtime")));
    let mut sidebar = SidebarState::default();
    let id = ProjectId::new("demo");
    sidebar
        .add_project(id.clone(), "demo", &project)
        .expect("project");
    sidebar.select_project(&id).expect("select");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
        .expect("state")
        .with_sidebar(sidebar)
        .with_provider_settings_path(user.join("config.toml"))
        .with_production_model(context, model.clone());
    (HeadlessWorkbench::new(state, [1200.0, 900.0]), model)
}

fn finish(harness: &mut HeadlessWorkbench<DemoSource>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.state().role_settings().is_saving()
        || harness.state().routing_settings().is_saving()
        || harness.state().project_role_profile_pending()
    {
        assert!(Instant::now() < deadline, "settings job timed out");
        harness.step();
        std::thread::yield_now();
    }
    harness.run();
}

#[test]
fn role_settings_open_on_project_profile_and_notice_which_profile_runs() {
    // Given: the active project selects the user's `fast` profile.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, runtime) = fixture(temp.path(), Some("fast"));
    assert_eq!(
        runtime.selected_model(Role::Worker, None),
        "accelerated/fast"
    );
    // When: opening role settings.
    harness.state_mut().open_role_settings();
    harness.run();
    // Then: the project's profile is edited and the notice says it is the one that runs.
    let profiles = &harness.state().role_settings().profiles;
    assert_eq!(profiles.names, ["default", "fast"]);
    assert_eq!(profiles.selected, "fast");
    assert!(
        harness.has_label(
            "Project demo uses role profile 'fast'. You are editing the profile it uses."
        )
    );
    // When: switching to the default profile.
    harness
        .state_mut()
        .apply_role_profile_action(RoleProfileAction::Select("default".into()));
    harness.run();
    // Then: the notice warns that edits do not reach the project and previews are hidden.
    assert_eq!(harness.state().role_settings().profiles.selected, "default");
    assert!(
        harness.has_label(
            "Project demo uses role profile 'fast'. Edits to 'default' do not affect it."
        )
    );
    assert!(harness.state().role_settings().resolved_previews.is_empty());
}

#[test]
fn new_profile_copies_selection_and_saves_and_deletes_only_that_profile() {
    // Given: the active project uses the default profile.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, runtime) = fixture(temp.path(), None);
    harness.state_mut().open_role_settings();
    harness.run();
    // When: creating `cheap` from the default profile through the modal.
    harness.state_mut().role_settings_mut().profiles.new_name = "cheap".into();
    harness.run();
    harness.click_label("New profile");
    harness.step();
    finish(&mut harness);
    // Then: it copies default's routes, stays open, and becomes the edited profile.
    assert_eq!(harness.state().role_settings().error, None);
    assert!(harness.state().role_settings().open);
    assert_eq!(harness.state().role_settings().profiles.selected, "cheap");
    let saved = config::Config::load_unresolved(&user_options(temp.path())).expect("saved");
    assert_eq!(saved.role_profiles["cheap"].routing, saved.routing);
    // When: binding worker to `fast` and saving.
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .worker
        .base
        .logical_model = Some("fast".into());
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);
    // Then: only `cheap` changes and the project's default runtime is unchanged.
    assert_eq!(harness.state().role_settings().error, None);
    let saved = config::Config::load_unresolved(&user_options(temp.path())).expect("saved");
    assert_eq!(
        saved.role_profiles["cheap"]
            .agents
            .worker
            .base
            .logical_model
            .as_deref(),
        Some("fast")
    );
    assert_eq!(saved.agents.worker.base.logical_model, None);
    assert_eq!(runtime.selected_model(Role::Worker, None), "local/base");
    // When: deleting `cheap` from the reopened modal.
    harness
        .state_mut()
        .open_role_settings_profile(Some("cheap"));
    harness.run();
    harness.click_label("Delete profile");
    harness.step();
    finish(&mut harness);
    // Then: the profile is gone and editing falls back to default.
    let saved = config::Config::load_unresolved(&user_options(temp.path())).expect("saved");
    assert!(!saved.role_profiles.contains_key("cheap"));
    assert!(saved.role_profiles.contains_key("fast"));
    assert_eq!(harness.state().role_settings().profiles.selected, "default");
}

#[test]
fn routing_settings_edit_the_selected_profile() {
    // Given: routing opened on the default profile.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path(), None);
    harness.state_mut().open_routing_settings();
    harness
        .state_mut()
        .apply_routing_profile_action(RoleProfileAction::Select("fast".into()));
    harness.run();
    // When: adding a route to `fast` and saving through the modal.
    let routing = harness.state().routing_settings();
    assert_eq!(routing.profiles.selected, "fast");
    assert_eq!(routing.routes.keys().collect::<Vec<_>>(), ["worker"]);
    harness
        .state_mut()
        .routing_settings_mut()
        .add_route("planner")
        .expect("route");
    harness.run();
    harness.click_label("Save routing");
    harness.step();
    finish(&mut harness);
    // Then: the route lands in `fast` only.
    assert_eq!(harness.state().routing_settings().validation_error, None);
    let saved = config::Config::load_unresolved(&user_options(temp.path())).expect("saved");
    assert!(
        saved.role_profiles["fast"]
            .routing
            .routes
            .contains_key("planner")
    );
    assert!(!saved.routing.routes.contains_key("planner"));
}

#[test]
fn project_settings_role_profile_writes_project_config_and_recomposes_active_model() {
    // Given: the active project on the default profile, with its settings dialog open.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, runtime) = fixture(temp.path(), None);
    harness
        .state_mut()
        .open_project_settings(ProjectId::new("demo"));
    harness.run();
    let ProjectDialog::Settings {
        role_profile,
        role_profiles,
        ..
    } = harness.state().project_dialog()
    else {
        panic!("settings dialog");
    };
    assert_eq!(role_profile, "default");
    assert_eq!(role_profiles, &["default", "fast"]);
    // When: picking `fast` in the Role profile combobox.
    harness.click_label(gui::panes::project_dialog::ROLE_PROFILE_LABEL);
    harness.run();
    harness.click_label("fast");
    harness.step();
    finish(&mut harness);
    // Then: the project config selects it and the active runtime switches to its routes.
    let project = temp.path().join("project");
    assert_eq!(
        config::project_role_profile(&project).expect("project config"),
        Some("fast".into())
    );
    assert_eq!(
        runtime.selected_model(Role::Worker, None),
        "accelerated/fast"
    );
    let ProjectDialog::Settings {
        role_profile,
        error,
        ..
    } = harness.state().project_dialog()
    else {
        panic!("settings dialog stays open");
    };
    assert_eq!(role_profile, "fast");
    assert_eq!(error, &None);
}
