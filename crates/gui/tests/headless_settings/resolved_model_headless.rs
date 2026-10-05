use std::sync::Arc;

use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::composer::ProviderStatus;
use gui::model::production::{ProductionModel, compose_production_model};
use runtime::compose::SwitchableModel;
use workspace_ui::{ModelPreference, ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path, resolved: bool) -> HeadlessWorkbench<DemoSource> {
    std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).expect("config directory");
    std::fs::write(
        config::project_main_config_path(root),
        r#"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["worker-model", "planner-model", "explicit-model"]
default_model = "worker-model"
[providers.planner]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["planner-model"]
default_model = "planner-model"
[agents.worker]
logical_model = "implementation"
[agents.orchestrator]
logical_model = "planning"
[routing.routes]
implementation = [{ profile = "local", model = "worker-model" }]
planning = [{ profile = "planner", model = "planner-model" }]
"#,
    )
    .expect("config");
    let context = ProductionModel {
        load_options: config::LoadOptions {
            project_dir: Some(root.into()),
            user_config_dir: Some(root.join("user")),
            read_env: false,
            ..Default::default()
        },
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(root.join("credentials"))
                .expect("credentials"),
        ),
        bus: Arc::new(event_bus::EventBus::new(32)),
        env: Arc::new(routing::MapEnv::new(
            [("TEST_KEY".into(), "test-secret".into())].into(),
        )),
    };
    let mut config = config::Config::load(&context.load_options).expect("load config");
    if !resolved {
        config.agents.worker.base.logical_model = Some("missing-route".into());
    }
    let model = compose_production_model(&config, &context).expect("production model");
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("resolved-model");
    sidebar
        .add_project(project.clone(), "test", root)
        .expect("project");
    sidebar.select_project(&project).expect("select");
    let thread = ThreadId::new("resolved-thread");
    sidebar
        .create_thread(thread.clone(), project, "thread")
        .expect("thread");
    sidebar.switch_thread(&thread).expect("switch");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("state")
        .with_sidebar(sidebar)
        .with_production_model(context, Arc::new(SwitchableModel::new(model)))
        .with_provider_status(ProviderStatus::Configured);
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

#[test]
fn automatic_mode_shows_resolved_profile_model_in_the_picker() {
    // Given: the worker binding selects its concrete profile/model.
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path(), true);
    // When: render the automatic-mode composer.
    harness.run();
    // Then: the picker names the routed default instead of a generic prompt.
    assert!(harness.has_label("Role: worker"));
    assert!(!harness.has_label("Select model"));
    assert!(
        harness.has_label("local/worker-model"),
        "composer: {:?}",
        harness.state().composer()
    );
    if let Some(path) = std::env::var_os("RESOLVED_MODEL_CAPTURE") {
        harness
            .capture()
            .expect("offscreen capture")
            .save_png(std::path::Path::new(&path))
            .expect("PNG");
    }
}

#[test]
fn switching_role_updates_resolved_label() {
    // Given: a worker composer with distinct role bindings.
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path(), true);
    harness.run();
    // When: the role button switches the active composer role.
    harness.click_label("Role: worker");
    harness.run();
    // Then: the orchestrator model replaces the worker model.
    assert!(harness.has_label("Role: orchestrator"));
    assert!(
        harness.has_label("planner/planner-model"),
        "composer: {:?}",
        harness.state().composer()
    );
    assert!(!harness.has_label("local/worker-model"));
}

#[test]
fn explicit_model_preference_hides_resolved_label() {
    // Given: the composer initially uses automatic routing.
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path(), true);
    harness.run();
    // When: the active thread selects an explicit model.
    harness
        .state_mut()
        .set_thread_model_preference(Some(ModelPreference {
            profile: "local".into(),
            model: Some("explicit-model".into()),
            reasoning_effort: None,
        }));
    harness.run();
    // Then: only the picker shows a model, without a stale resolved label.
    assert!(!harness.has_label("local/worker-model"));
    assert!(harness.has_label("local / explicit-model"));
}

#[test]
fn failed_resolution_shows_verbatim_logical_model() {
    // Given: a binding whose logical route does not exist.
    let temp = tempfile::tempdir().expect("root");
    let mut harness = workbench(temp.path(), false);
    // When: render the automatic-mode composer.
    harness.run();
    // Then: the runtime diagnostic is visible without substitution.
    assert!(harness.has_label("unresolved:missing-route"));
}
