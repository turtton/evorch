use std::sync::Arc;

use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use runtime::compose::{SwitchableModel, UnconfiguredModel};

pub(super) fn fixture(
    root: &std::path::Path,
) -> (WorkbenchState<DemoSource>, Arc<SwitchableModel>) {
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
"#,
    )
    .expect("fixture");
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
            [("TEST_KEY".into(), "secret".into())].into(),
        )),
    };
    let model = Arc::new(SwitchableModel::new(Arc::new(UnconfiguredModel)));
    let state = WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
        .expect("state")
        .with_provider_settings_path(root.join("evorch.toml"))
        .with_production_model(context, model.clone());
    (state, model)
}

pub(super) fn finish(harness: &mut HeadlessWorkbench<DemoSource>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while harness.state().routing_settings().is_saving() {
        assert!(std::time::Instant::now() < deadline, "save timeout");
        harness.step();
        std::thread::yield_now();
    }
    harness.run();
}
