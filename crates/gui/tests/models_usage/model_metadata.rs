use std::sync::{Arc, mpsc};

use config::{MetadataSource, ModelEntryConfig, ModelPresetConfig};
use egui_kittest::{Harness, kittest::Queryable};
use gui::model::model_catalog::{CatalogBackend, CatalogFetch, CatalogRequest, CatalogState};
use gui::model::model_metadata::MetadataSources;
use gui::model::provider_settings::OpenAiEditorModel;

#[path = "model_metadata/provider_types.rs"]
mod provider_types;

fn entry() -> ModelEntryConfig {
    ModelEntryConfig {
        id: "test-model".into(),
        enabled: true,
        kind: None,
        metadata_source: Some(MetadataSource::ModelsDev),
        preset: None,
        metadata_ref: None,
        context_window: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        fast_price: None,
        price_tiers: None,
        effort_levels: None,
        edit_tool: None,
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
    let labels = sources.labels(&entry, "test-provider", None);
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
        sources.labels(&entry(), "test-provider", None)[0],
        "64000 ctx (models.dev)"
    );
    assert_eq!(
        sources.labels(&entry(), "wrong-provider", None)[0],
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
            for label in sources.labels(entry, "Crof", None) {
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
        sources.labels(&entry, "test-provider", None)[0],
        "256000 ctx (manual override)"
    );
    assert_eq!(
        sources.labels(&entry, "test-provider", None)[3],
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
    ) -> Result<CatalogFetch, String> {
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
    // A spinner intentionally never settles; drive frames until the worker finishes.
    harness.step();
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
        CatalogRequest::ForceRefresh
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while harness.state().is_busy() {
        assert!(
            std::time::Instant::now() < deadline,
            "refresh did not finish"
        );
        harness.step();
        std::thread::yield_now();
    }
    assert_eq!(harness.state().error.as_deref(), Some("offline fixture"));
}

#[test]
fn config_roundtrip_with_preset_keeps_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = config::project_main_config_path(dir.path());
    std::fs::create_dir_all(dir.path().join(config::PROJECT_CONFIG_DIR)).expect("config directory");
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
        user_config_dir: Some(dir.path().join(config::PROJECT_CONFIG_DIR)),
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
            for label in sources.labels(entry, "test-provider", None) {
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

#[test]
fn cursor_rows_name_the_docs_or_api_equivalent_origin() {
    // Given: a catalog that lists the public API model but not Cursor's suffixed ID.
    let dir = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        dir.path().join("models-dev.json"),
        format!(
            r#"{{"fetched_at":{now},"api":{{"anthropic":{{"models":{{"claude-opus-5-5":{{"id":"claude-opus-5-5","limit":{{"context":1000000,"output":64000}},"cost":{{"input":4.0,"output":20.0}}}}}}}},"openai":{{"models":{{"gpt-5":{{"id":"gpt-5","limit":{{"context":400000,"output":128000}},"cost":{{"input":1.25,"output":10.0}}}}}}}}}}}}"#
        ),
    )
    .unwrap();
    let catalog = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(catalog::ModelCatalog::load_or_refresh(dir.path()))
        .unwrap();
    let presets = presets();
    let sources = MetadataSources {
        presets: &presets,
        catalog: Some(&catalog),
    };
    let cursor = Some(config::ProviderTypeConfig::Cursor);
    let labels = |id: &str| sources.labels(&ModelEntryConfig::enabled(id), "cursor", cursor);

    // When/Then: an undocumented variant shows the API model's values as API-equivalent,
    // Cursor's own model shows the docs, and a documented third-party model keeps the
    // API prices but Cursor's window.
    let api = "models.dev, API-equivalent";
    assert_eq!(
        labels("gpt-5-high"),
        [
            format!("400000 ctx ({api})"),
            format!("128000 max output ({api})"),
            format!("$1.25/M input ({api})"),
            format!("$10/M output ({api})"),
        ]
    );
    let opus = labels("claude-opus-5-5-high");
    assert_eq!(opus[0], "300000 ctx (Cursor docs)");
    assert_eq!(opus[2], format!("$4/M input ({api})"));
    let composer = labels("composer-2.5-fast");
    assert_eq!(composer[0], "200000 ctx (Cursor docs)");
    assert_eq!(composer[2], "$3/M input (Cursor docs)");
    assert_eq!(composer[3], "$15/M output (Cursor docs)");
}

struct ScriptedBackend(std::sync::Mutex<Option<Result<CatalogFetch, String>>>);

#[async_trait::async_trait]
impl CatalogBackend for ScriptedBackend {
    async fn fetch(
        &self,
        _request: CatalogRequest,
        _dir: &std::path::Path,
    ) -> Result<CatalogFetch, String> {
        self.0.lock().unwrap().take().unwrap()
    }
}

fn run_to_completion(state: &mut CatalogState, request: CatalogRequest) {
    state.start(request);
    // The worker always finishes; a hang is caught by the test runner, not by a deadline here.
    while state.is_busy() {
        state.poll();
        std::thread::yield_now();
    }
}

fn scripted(fetch: Result<CatalogFetch, String>) -> CatalogState {
    let dir = tempfile::tempdir().unwrap();
    CatalogState::with_backend(
        dir.path().into(),
        Arc::new(ScriptedBackend(std::sync::Mutex::new(Some(fetch)))),
    )
}

// Given: a forced refresh that fetched models.dev
// When: whichever poll sees it (the toolbar may poll before the frame does)
// Then: the new data is kept for exactly one hand-off to the running models.
#[test]
fn refreshed_catalog_is_handed_out_once_regardless_of_who_polled() {
    let mut state = scripted(Ok(CatalogFetch::Refreshed(catalog())));
    run_to_completion(&mut state, CatalogRequest::ForceRefresh);
    assert!(state.error.is_none());
    assert!(state.fetched_at.is_some());
    let published = state.take_published().expect("refreshed data is published");
    assert!(Arc::ptr_eq(&published, state.catalog.as_ref().unwrap()));
    assert!(state.take_published().is_none());
}

// Given: a forced refresh that failed while a cache was readable
// When: the outcome arrives / Then: the user is told, the cache keeps being shown and
// nothing is published to the running models.
#[test]
fn retained_cache_is_reported_and_not_published() {
    let mut state = scripted(Ok(CatalogFetch::CacheRetained {
        catalog: catalog(),
        reason: "catalog HTTP request failed: offline".into(),
    }));
    run_to_completion(&mut state, CatalogRequest::ForceRefresh);
    assert!(
        state
            .error
            .as_deref()
            .is_some_and(|error| error.contains("offline") && error.contains("cached data"))
    );
    assert!(state.catalog.is_some());
    assert!(state.take_published().is_none());
}

// Given: a plain cache read at startup / When: it arrives
// Then: it is shown but not published: the models read the same cache themselves.
#[test]
fn loaded_cache_is_not_published() {
    let mut state = scripted(Ok(CatalogFetch::Loaded(catalog())));
    run_to_completion(&mut state, CatalogRequest::Load);
    assert!(state.catalog.is_some());
    assert!(state.take_published().is_none());
}

#[test]
fn toolbar_states_that_a_refresh_only_applies_to_new_runs() {
    let harness = Harness::new_ui_state(
        gui::panes::model_metadata::catalog_toolbar,
        CatalogState::with_backend(
            tempfile::tempdir().unwrap().path().into(),
            Arc::new(FailedBackend(mpsc::channel().0)),
        ),
    );
    harness.get_by_label(gui::panes::model_metadata::APPLIES_TO_NEW_RUNS);
}
