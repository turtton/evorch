use std::sync::Arc;

use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use runtime::compose::{SwitchableModel, UnconfiguredModel};
use runtime::{AgentModel, Role};

fn fixture(root: &std::path::Path) -> (HeadlessWorkbench<DemoSource>, Arc<SwitchableModel>) {
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
    let model = Arc::new(SwitchableModel::new(Arc::new(UnconfiguredModel)));
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("state")
            .with_provider_settings_path(root.join("evorch.toml"))
            .with_production_model(context, model.clone());
    state.open_role_settings();
    (HeadlessWorkbench::new(state, [1200.0, 900.0]), model)
}

fn finish(harness: &mut HeadlessWorkbench<DemoSource>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while harness.state().role_settings().is_saving() {
        assert!(std::time::Instant::now() < deadline, "save timeout");
        harness.step();
        std::thread::yield_now();
    }
    harness.run();
}

#[test]
fn role_binding_edit_saves_toml_and_reloads_runtime() {
    // Given: a production reload context and two distinct routes.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, model) = fixture(temp.path());
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .worker
        .logical_model = Some("fast".into());
    harness.run();
    // When: saving through the actual modal.
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);
    // Then: persisted, reseeded and live runtime values agree.
    assert_eq!(harness.state().role_settings().error, None);
    let saved = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("saved config");
    assert_eq!(saved.agents.worker.logical_model.as_deref(), Some("fast"));
    assert_eq!(harness.state().role_settings().agents, saved.agents);
    assert_eq!(model.selected_model(Role::Worker), "accelerated/fast");
}

#[test]
fn category_override_editable_per_role() {
    // Given: the worker category editor.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    harness.run();
    harness.click_label("Worker");
    harness.run();
    harness.click_label("quick");
    harness.run();
    // When: a category-specific model is selected and saved.
    harness.click_label("quick logical model");
    harness.run();
    harness.click_label("fast");
    harness.run();
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);
    // Then: only the worker quick binding changes.
    let agents = &harness.state().role_settings().agents;
    assert_eq!(
        agents.worker.categories["quick"].logical_model.as_deref(),
        Some("fast")
    );
    assert_eq!(agents.worker.logical_model, None);
    assert!(agents.explorer.categories.is_empty());
}

#[test]
fn librarian_row_saves_when_model_selected() {
    // Given: the real role settings modal with distinct routes.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, model) = fixture(temp.path());
    harness.run();
    // When: editing the librarian row and saving through its controls.
    harness.click_label("Librarian");
    harness.run();
    harness.click_label("Role logical model");
    harness.run();
    harness.click_label("fast");
    harness.run();
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);
    // Then: the saved binding is reseeded and used by the live runtime.
    assert_eq!(harness.state().role_settings().error, None);
    assert_eq!(
        harness
            .state()
            .role_settings()
            .agents
            .binding_for("librarian", None)
            .expect("saved librarian")
            .logical_model,
        "fast"
    );
    assert_eq!(model.selected_model(Role::Librarian), "accelerated/fast");
}

#[test]
fn validation_error_blocks_save() {
    // Given: unknown and empty explicit model assignments.
    for name in ["unknown", ""] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut harness, _) = fixture(temp.path());
        let before = std::fs::read(temp.path().join("evorch.toml")).expect("read");
        harness
            .state_mut()
            .role_settings_mut()
            .agents
            .worker
            .logical_model = Some(name.into());
        harness.run();
        // When: saving is requested.
        harness.click_label("Save role settings");
        harness.run();
        // Then: validation is visible and disk remains untouched.
        let error = harness
            .state()
            .role_settings()
            .error
            .as_deref()
            .expect("error");
        assert!(harness.has_label(error));
        assert_eq!(
            std::fs::read(temp.path().join("evorch.toml")).expect("read"),
            before
        );
    }
}

#[test]
fn role_settings_geometry_matrix() {
    // Given: supported viewport sizes and DPI scales.
    for size in [[960.0, 600.0], [1200.0, 900.0]] {
        for dpi in [1.0, 1.5, 2.0] {
            let temp = tempfile::tempdir().expect("temp");
            let (mut original, _) = fixture(temp.path());
            original.state_mut().role_settings_mut().open = false;
            let mut state =
                WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
                    .expect("state")
                    .with_provider_settings_path(temp.path().join("evorch.toml"));
            state.open_role_settings();
            let mut harness = HeadlessWorkbench::with_pixels_per_point(state, size, dpi);
            harness.run();
            // When: the deepest category controls are expanded.
            harness.click_label("Worker");
            harness.run();
            harness.click_label("quick");
            harness.run();
            // Then: save stays visible and category controls are scroll reachable.
            for label in ["Save role settings", "Cancel", "quick logical model"] {
                harness.scroll_label_into_view(label);
                harness.run();
                let rects = harness.label_rects(label);
                assert!(!rects.is_empty(), "{label}");
                assert!(
                    rects
                        .iter()
                        .all(|rect| harness.screen_rect().contains_rect(*rect)),
                    "{label}: {rects:?}"
                );
            }
        }
    }
}

#[test]
fn menu_opens_role_settings_and_cancel_discards_edits() {
    // Given: a closed role editor with an unsaved change.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .worker
        .logical_model = Some("fast".into());
    harness.run();
    harness.click_label("Cancel");
    harness.run();
    // When: reopening through the settings menu.
    harness.click_label("Workbench settings");
    harness.run();
    harness.click_label("Agent roles");
    harness.run();
    // Then: disk values replace discarded edits.
    assert!(harness.has_label("Agent role settings"));
    assert_eq!(
        harness.state().role_settings().agents.worker.logical_model,
        None
    );
}

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_role_settings_png_evidence() {
    // Given: an isolated editor with the category section expanded.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    harness.run();
    harness.click_label("Librarian");
    harness.run();
    harness.click_label("research");
    harness.run();
    // When: capturing the real egui surface offscreen.
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gui-evidence");
    std::fs::create_dir_all(&output).expect("evidence directory");
    frame
        .save_png(&output.join("role-settings.png"))
        .expect("PNG");
    // Then: the new screen and primary action are present at the expected dimensions.
    assert_eq!((frame.width, frame.height), (1200, 900));
    assert!(harness.has_label("Save role settings"));
}
