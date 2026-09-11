use std::sync::Arc;

use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::composer::ProviderStatus;
use gui::model::production::{ProductionModel, compose_production_model};
use runtime::compose::SwitchableModel;
use workspace_ui::{ModelPreference, ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path, configured: bool) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("demo");
    sidebar.add_project(project.clone(), "demo", root).unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["thread-1", "thread-2"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("thread-1")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"));
    if configured {
        std::fs::write(
            root.join("evorch.toml"),
            r#"
[providers.local]
type = "openai-compatible"
base_url = "http://localhost:11434/v1"
models = ["model-a", "model-b"]
default_model = "model-a"
[providers.remote]
type = "openai-compatible"
base_url = "https://example.test/v1"
models = ["model-c"]
default_model = "model-c"
"#,
        )
        .unwrap();
        let context = ProductionModel {
            load_options: config::LoadOptions {
                project_dir: Some(root.to_owned()),
                user_config_dir: Some(root.join("user")),
                read_env: false,
                ..Default::default()
            },
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(root.join("credentials")).unwrap(),
            ),
            bus: Arc::new(event_bus::EventBus::new(32)),
            env: Arc::new(
                [("ANTHROPIC_API_KEY", "test-secret")]
                    .into_iter()
                    .collect::<routing::MapEnv>(),
            ),
        };
        let config = config::Config::load(&context.load_options).unwrap();
        let model = compose_production_model(&config, &context).unwrap();
        state = state
            .with_production_model(context, Arc::new(SwitchableModel::new(model)))
            .with_provider_status(ProviderStatus::Configured);
    }
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

#[test]
fn picker_lists_profiles_and_models_from_catalog() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness.run();
    // When
    harness.click_label("Select model");
    harness.run();
    // Then
    for label in ["local / model-a", "local / model-b", "remote / model-c"] {
        assert!(harness.has_label(label));
    }
}

#[test]
fn selecting_model_persists_on_thread() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness.run();
    harness.click_label("Select model");
    harness.run();
    // When
    harness.click_label("local / model-b");
    harness.run();
    // Then
    let threads = &harness.state().sidebar().threads;
    assert_eq!(
        threads[0].model_preference,
        Some(ModelPreference {
            profile: "local".into(),
            model: Some("model-b".into()),
        })
    );
    assert_eq!(threads[1].model_preference, None);
    assert!(harness.state().issued().is_empty());
    let restored = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert_eq!(
        restored.threads[0].model_preference,
        threads[0].model_preference
    );
}

#[test]
fn clearing_selection_restores_automatic_routing() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness
        .state_mut()
        .set_thread_model_preference(Some(ModelPreference {
            profile: "local".into(),
            model: Some("model-b".into()),
        }));
    harness.run();
    harness.click_label("local / model-b");
    harness.run();
    // When
    harness.click_label("Automatic routing");
    harness.run();
    // Then
    assert_eq!(harness.state().sidebar().threads[0].model_preference, None);
}

#[test]
#[ignore = "writes picker PNG evidence using an offscreen GPU adapter"]
fn capture_model_picker_evidence() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), true);
    harness
        .state_mut()
        .set_thread_model_preference(Some(ModelPreference {
            profile: "local".into(),
            model: Some("model-a".into()),
        }));
    harness.run();
    // When
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    frame
        .save_png(std::path::Path::new("/tmp/opencode/w-picker.png"))
        .unwrap();
    harness.click_label("local / model-a");
    harness.run();
    // Then
    assert!(harness.has_label("remote / model-c"));
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    frame
        .save_png(std::path::Path::new("/tmp/opencode/w-picker-open.png"))
        .unwrap();
}

#[test]
fn picker_disabled_without_profiles_offers_settings() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench(temp.path(), false);
    harness.run();
    // When
    harness.click_label("Open Settings");
    harness.run();
    // Then
    assert!(harness.state().provider_settings().open);
    assert!(harness.has_label("Select model"));
}
