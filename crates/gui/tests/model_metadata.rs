use std::sync::{Arc, mpsc};

use config::{MetadataSource, ModelEntryConfig, ModelPresetConfig};
use egui_kittest::{Harness, kittest::Queryable};
use gui::model::model_catalog::{CatalogBackend, CatalogRequest, CatalogState};
use gui::model::model_metadata::MetadataSources;
use gui::model::provider_settings::OpenAiEditorModel;

fn entry() -> ModelEntryConfig {
    ModelEntryConfig {
        id: "test-model".into(),
        enabled: true,
        metadata_source: Some(MetadataSource::ModelsDev),
        preset: None,
        metadata_ref: None,
        context_window: None,
    }
}

fn presets() -> std::collections::BTreeMap<String, ModelPresetConfig> {
    [(
        "large".into(),
        ModelPresetConfig {
            context_window: Some(128_000),
            max_output_tokens: Some(8_000),
            input_price_per_million_usd: Some(2.5),
            ..Default::default()
        },
    )]
    .into()
}

fn catalog() -> catalog::ModelCatalog {
    let dir = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(dir.path().join("models-dev.json"), format!(r#"{{"fetched_at":{now},"api":{{"test-provider":{{"models":{{"test-model":{{"id":"test-model","limit":{{"context":64000,"output":4000}},"cost":{{"input":1.0,"output":3.0}}}}}}}}}}}}"#)).unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(catalog::ModelCatalog::load_or_refresh(dir.path()))
        .unwrap()
}

#[test]
fn model_row_shows_resolved_context_window_from_preset() {
    let presets = presets();
    let mut entry = entry();
    entry.preset = Some("large".into());
    let sources = MetadataSources {
        presets: &presets,
        catalog: None,
    };
    let labels = sources.labels(&entry, "test-provider");
    assert_eq!(labels[0], "128000 ctx (Preset: large)");
    assert_eq!(labels[1], "8000 max output (Preset: large)");
    assert_eq!(labels[2], "$2.5/M input (Preset: large)");
}

#[test]
fn model_row_shows_catalog_origin() {
    let presets = presets();
    let catalog = catalog();
    let sources = MetadataSources {
        presets: &presets,
        catalog: Some(&catalog),
    };
    assert_eq!(
        sources.labels(&entry(), "test-provider")[0],
        "64000 ctx (models.dev)"
    );
    assert_eq!(
        sources.labels(&entry(), "wrong-provider")[0],
        "64000 ctx (models.dev)"
    );
}

#[test]
fn model_row_auto_catalog_and_unknown_guidance() {
    // Given: a provider-returned model with no metadata configuration.
    let presets = presets();
    let catalog = catalog();
    let sources = MetadataSources {
        presets: &presets,
        catalog: Some(&catalog),
    };
    let mut entry = entry();
    entry.metadata_source = None;
    // When: rendering metadata through the existing GUI surface.
    let mut harness = Harness::new_ui_state(
        |ui, entry| {
            for label in sources.labels(entry, "Crof") {
                ui.label(label);
            }
        },
        entry,
    );
    // Then: all catalog fields agree, and a miss offers a next action.
    harness.get_by_label("64000 ctx (models.dev)");
    harness.get_by_label("4000 max output (models.dev)");
    harness.get_by_label("$1/M input (models.dev)");
    harness.get_by_label("$3/M output (models.dev)");
    harness.state_mut().id = "missing-model".into();
    harness.run();
    harness.get_by_label("Unknown ctx (default) - set preset or metadata_ref");
}

#[test]
fn model_row_manual_override_shown() {
    let presets = presets();
    let catalog = catalog();
    let sources = MetadataSources {
        presets: &presets,
        catalog: Some(&catalog),
    };
    let mut entry = entry();
    entry.preset = Some("large".into());
    entry.context_window = Some(256_000);
    assert_eq!(
        sources.labels(&entry, "test-provider")[0],
        "256000 ctx (manual override)"
    );
    assert_eq!(
        sources.labels(&entry, "test-provider")[3],
        "$3/M output (models.dev)"
    );
}

struct FailedBackend(mpsc::Sender<CatalogRequest>);

#[async_trait::async_trait]
impl CatalogBackend for FailedBackend {
    async fn fetch(
        &self,
        request: CatalogRequest,
        _dir: &std::path::Path,
    ) -> Result<catalog::ModelCatalog, String> {
        self.0.send(request).unwrap();
        Err("offline fixture".into())
    }
}

#[test]
fn refresh_button_triggers_force_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel();
    let state = CatalogState::with_backend(dir.path().into(), Arc::new(FailedBackend(tx)));
    let mut harness = Harness::new_ui_state(gui::panes::model_metadata::catalog_toolbar, state);
    harness.get_by_label("Model catalog Refresh").click();
    harness.run();
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
        CatalogRequest::ForceRefresh
    );
}

#[test]
fn config_roundtrip_with_preset_keeps_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evorch.toml");
    std::fs::write(&path, "[model_presets.large]\ncontext_window = 128000\n").unwrap();
    let mut editor = OpenAiEditorModel {
        name: "test-provider".into(),
        base_url: "https://example.invalid/v1".into(),
        credential_mode: gui::model::provider_settings::CredentialMode::Env,
        api_key_env: "TEST_API_KEY".into(),
        default_model: "test-model".into(),
        models: vec![entry()],
        ..Default::default()
    };
    editor.models[0].preset = Some("large".into());
    editor.models[0].metadata_ref = Some("test-provider/test-model".into());
    editor.models[0].context_window = Some(256_000);
    config::save_openai_compatible_provider(&path, &editor.to_input()).unwrap();
    let loaded = config::Config::load(&config::LoadOptions {
        project_dir: Some(dir.path().into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(loaded.providers["test-provider"].models, editor.models);
    assert_eq!(loaded.model_presets["large"].context_window, Some(128_000));
}

#[test]
fn metadata_controls_edit_and_clear_override() {
    let presets = presets();
    let mut harness = Harness::new_ui_state(
        |ui, entry| {
            let sources = MetadataSources {
                presets: &presets,
                catalog: None,
            };
            gui::panes::model_metadata::model_metadata(ui, entry, &sources);
            for label in sources.labels(entry, "test-provider") {
                ui.label(label);
            }
        },
        entry(),
    );
    harness.get_by_label("Preset").click();
    harness.run();
    harness.get_by_label("large").click();
    harness.run();
    assert_eq!(harness.state().preset.as_deref(), Some("large"));
    harness.get_by_label("128000 ctx (Preset: large)");
    harness.get_by_label("Context window override").click();
    harness.run();
    harness
        .get_by_label("Context window override")
        .type_text("256000");
    harness.run();
    harness.get_by_label("256000 ctx (manual override)");
    harness.get_by_label("Context window override").click();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    harness.key_press(egui::Key::Backspace);
    harness.run();
    assert_eq!(harness.state().context_window, None);
    harness.get_by_label("128000 ctx (Preset: large)");
}

#[test]
fn refresh_failure_keeps_loaded_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel();
    let mut state = CatalogState::with_backend(dir.path().into(), Arc::new(FailedBackend(tx)));
    state.catalog = Some(Arc::new(catalog()));
    state.start(CatalogRequest::ForceRefresh);
    rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while state.is_busy() {
        assert!(std::time::Instant::now() < deadline);
        state.poll();
        std::thread::yield_now();
    }
    assert_eq!(state.error.as_deref(), Some("offline fixture"));
    assert_eq!(
        state
            .catalog
            .as_ref()
            .unwrap()
            .find("test-provider", "test-model")
            .unwrap()
            .context_window,
        Some(64_000)
    );
}
