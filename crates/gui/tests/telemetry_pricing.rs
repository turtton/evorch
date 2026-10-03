use config::{Config, ModelEntryConfig, ProviderProfileConfig};
use event_bus::{Event, ProviderEvent};
use gui::model::{provider_settings::ProviderSettingsModel, telemetry::TelemetryOverlay};
use std::sync::Arc;

#[test]
fn cost_includes_known_prices_when_cache_prices_are_unknown() {
    // Given: input/output prices without cache prices.
    let usage = gui::model::telemetry::TokenUsage {
        input: 98_300,
        output: 15_400,
        cache_read: 78_300,
        cache_write: 1_700,
    };
    let pricing = config::types::provider::ModelPricing {
        input: Some(0.5),
        output: Some(2.0),
        cache_read: None,
        cache_write: None,
    };
    // When: the cost is estimated for cached usage.
    let cost = usage.estimated_cost(Some(pricing));
    // Then: the input and output portion remains visible.
    assert_eq!(cost, Some(0.03995));
}

fn completed(model: &str) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: model.into(),
        provider: "vendor".into(),
        profile: Some("local".into()),
        protocol: "fixture".into(),
        model: model.into(),
        streaming: true,
        duration_ms: 5_000,
        input_tokens: 98_300,
        output_tokens: 15_400,
        cache_read_tokens: 78_300,
        cache_write_tokens: 1_700,
        finish_reason: "stop".into(),
        run_id: Some("run-1".into()),
    })
}

#[test]
fn zero_usage_cost_is_zero_when_pricing_exists() {
    let usage = gui::model::telemetry::TokenUsage::default();
    let pricing = config::types::provider::ModelPricing {
        input: Some(0.5),
        output: Some(2.0),
        cache_read: Some(0.01),
        cache_write: Some(0.25),
    };
    assert_eq!(usage.estimated_cost(Some(pricing)), Some(0.0));
}

#[test]
fn run_cost_survives_when_another_priced_model_has_zero_usage() {
    let mut config = Config::default();
    let models = ["model", "zero"].map(|name| {
        let mut entry = ModelEntryConfig::enabled(name);
        entry.input_price = Some(0.5);
        entry.output_price = Some(2.0);
        entry
    });
    config.providers.insert(
        "local".into(),
        ProviderProfileConfig {
            models: models.into(),
            ..ProviderProfileConfig::default()
        },
    );
    let settings = ProviderSettingsModel::seed_from_config(&config);
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed("model"));
    overlay.apply_event(&Event::new(ProviderEvent::RequestCompleted {
        request_id: "zero".into(),
        provider: "vendor".into(),
        profile: Some("local".into()),
        protocol: "fixture".into(),
        model: "zero".into(),
        streaming: true,
        duration_ms: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some("run-1".into()),
    }));
    assert_eq!(overlay.estimated_cost("run-1", &settings), Some(0.03995));
    overlay.refresh_costs(&settings);
    assert_eq!(overlay.cost("run-1"), Some(0.03995));
}

fn settings() -> ProviderSettingsModel {
    let mut config = Config::default();
    let mut entry = ModelEntryConfig::enabled("model");
    entry.input_price = Some(0.5);
    entry.output_price = Some(2.0);
    entry.cache_read_price = Some(0.01);
    entry.cache_write_price = Some(0.25);
    config.providers.insert(
        "local".into(),
        ProviderProfileConfig {
            models: vec![entry],
            ..ProviderProfileConfig::default()
        },
    );
    ProviderSettingsModel::seed_from_config(&config)
}

#[test]
fn completed_costs_accumulate_by_profile_and_model() {
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed("model"));
    overlay.apply_event(&completed("model"));
    overlay.refresh_costs(&settings());
    assert!((overlay.cost("run-1").expect("cost") - 0.082316).abs() < 1e-10);
    overlay.apply_event(&completed("unknown"));
    overlay.refresh_costs(&settings());
    assert_eq!(overlay.cost("run-1"), None);
}

#[tokio::test]
async fn catalog_fills_missing_fields_but_static_prices_win() {
    let dir = tempfile::tempdir().expect("cache directory");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    std::fs::write(dir.path().join("models-dev.json"), format!(r#"{{"fetched_at":{now},"api":{{"vendor":{{"models":{{"model":{{"id":"model","cost":{{"input":10.0,"output":2.0,"cache_read":0.01,"cache_write":0.25}}}}}}}}}}}}"#)).expect("cache fixture");
    let mut config = Config::default();
    let mut entry = ModelEntryConfig::enabled("model");
    entry.input_price = Some(0.5);
    config.providers.insert(
        "local".into(),
        ProviderProfileConfig {
            models: vec![entry],
            ..ProviderProfileConfig::default()
        },
    );
    let mut settings = ProviderSettingsModel::seed_from_config(&config);
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed("model"));
    assert_eq!(overlay.estimated_cost("run-1", &settings), Some(0.00915));
    settings.catalog.catalog = Some(Arc::new(
        catalog::ModelCatalog::load_or_refresh(dir.path())
            .await
            .expect("catalog"),
    ));
    overlay.refresh_costs(&settings);
    assert!((overlay.cost("run-1").expect("cost") - 0.041158).abs() < 1e-10);
}

#[test]
fn telemetry_usage_renders_in_thread_subagents_pane() {
    use egui_kittest::{Harness, kittest::Queryable};
    use gui::model::tasks::{AgentRunSource, TasksModel};
    struct Source;
    impl AgentRunSource for Source {
        fn list(&self) -> Vec<runtime::AgentSummary> {
            vec![runtime::AgentSummary {
                run_id: runtime::RunId::new(1),
                parent_run_id: Some(runtime::RunId::new(0)),
                name: "worker".into(),
                role_name: "worker".into(),
                phase: event_bus::AgentRunPhase::Running,
                model: "model".into(),
                category: None,
            }]
        }
    }
    let mut tasks = TasksModel::new(Source);
    tasks.refresh();
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed("model"));
    overlay.refresh_costs(&settings());
    let mut harness = Harness::new_ui(move |ui| {
        gui::panes::agents::subagents_pane(
            ui,
            &tasks,
            &overlay,
            &Default::default(),
            &["run-1".into()],
        );
    });
    harness.run();
    for label in ["worker", "98300 / 15400"] {
        assert!(harness.query_by_label(label).is_some(), "missing {label}");
    }
}

#[test]
#[ignore = "L4 offscreen PNG evidence; requires a graphics adapter"]
fn capture_telemetry_cost_png() {
    use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
    use workspace_ui::{ProjectId, SidebarState, ThreadId};
    let source = DemoSource(vec![runtime::AgentSummary {
        run_id: runtime::RunId::new(1),
        parent_run_id: Some(runtime::RunId::new(0)),
        name: "worker".into(),
        role_name: "worker".into(),
        phase: event_bus::AgentRunPhase::Running,
        model: "model".into(),
        category: None,
    }]);
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("demo");
    sidebar
        .add_project(project.clone(), "demo", std::path::Path::new("/tmp"))
        .expect("project");
    sidebar.select_project(&project).expect("select project");
    let thread = ThreadId::new("cost-evidence");
    sidebar
        .create_thread(thread.clone(), project, "Cost evidence")
        .expect("thread");
    sidebar.switch_thread(&thread).expect("select thread");
    sidebar.threads[0].run_ids.push("run-1".into());
    let mut state = WorkbenchState::new(source, &workspace_ui::UiSettings::default())
        .expect("state")
        .with_sidebar(sidebar)
        .with_provider_settings(settings());
    // The billed ratio is 80%; retention compares against an 80K-token previous cache.
    state.apply_events([
        Event::new(ProviderEvent::CacheReuseObserved {
            request_id: "model".into(),
            cache_read_tokens: 78_300,
            comparison: event_bus::CacheComparison::Compared {
                previous_request_id: "previous".into(),
                previous_cache_tokens: 80_000,
            },
            run_id: Some("run-1".into()),
        }),
        completed("model"),
    ]);
    let path = state
        .dock()
        .find_tab(&workspace_ui::PanelId::new("agent-main"))
        .expect("conversation tab");
    state
        .dock_mut()
        .set_active_tab(path)
        .expect("activate conversation");
    let mut harness = HeadlessWorkbench::new(state, [1280.0, 720.0]);
    harness.run();
    assert!(harness.has_label("$0.041"));
    assert!(harness.has_label("cache 98% (avg 98%)"));
    if let Some(frame) = gui::evidence::capture_or_skip(&mut harness) {
        let path = std::env::var_os("EVORCH_TELEMETRY_PNG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("evorch-telemetry-{}.png", std::process::id()))
            });
        frame.save_png(&path).expect("PNG saved");
        eprintln!("telemetry evidence: {}", path.display());
    }
}
