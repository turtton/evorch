use std::sync::Arc;

use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use runtime::compose::SwitchableModel;

pub fn fixture(root: &std::path::Path) -> (HeadlessWorkbench<DemoSource>, Arc<SwitchableModel>) {
    std::fs::write(
        root.join("evorch.toml"),
        r#"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["base", "fast"]
default_model = "base"
[providers.accelerated]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["fast"]
default_model = "fast"
[routing.routes]
worker = [{ profile = "local", model = "base" }]
fast = [{ profile = "accelerated", model = "fast" }]
"#,
    )
    .expect("fixture config");
    open_fixture(root)
}

pub fn open_fixture(
    root: &std::path::Path,
) -> (HeadlessWorkbench<DemoSource>, Arc<SwitchableModel>) {
    sized_fixture(root, [1200.0, 900.0])
}

pub fn sized_fixture(
    root: &std::path::Path,
    size: [f32; 2],
) -> (HeadlessWorkbench<DemoSource>, Arc<SwitchableModel>) {
    let context = gui::model::production::ProductionModel {
        load_options: config::LoadOptions {
            project_dir: Some(root.into()),
            user_config_dir: Some(root.join("user")),
            read_env: false,
            ..Default::default()
        },
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(root.join("credentials"))
                .expect("store"),
        ),
        bus: Arc::new(event_bus::EventBus::new(32)),
        env: Arc::new(routing::MapEnv::new(
            [("TEST_KEY".into(), "test-secret".into())].into(),
        )),
    };
    let model = Arc::new(SwitchableModel::new(context.reload().expect("runtime")));
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("state")
            .with_provider_settings_path(root.join("evorch.toml"))
            .with_production_model(context, model.clone());
    state.open_role_settings();
    (HeadlessWorkbench::new(state, size), model)
}

pub fn finish(harness: &mut HeadlessWorkbench<DemoSource>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while harness.state().role_settings().is_saving() {
        assert!(std::time::Instant::now() < deadline, "save timeout");
        harness.step();
        std::thread::yield_now();
    }
    harness.run();
}
