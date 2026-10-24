use config::{Config, ModelEntryConfig, ProviderProfileConfig};
use event_bus::{Event, ProviderEvent};
use gui::model::{provider_settings::ProviderSettingsModel, telemetry::TelemetryOverlay};
use std::sync::Arc;

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
        service_tier: None,
        run_id: Some("run-1".into()),
        purpose: None,
        reasoning_tokens: None,
    })
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
        service_tier: None,
        run_id: Some("run-1".into()),
        purpose: None,
        reasoning_tokens: None,
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
    assert!(!overlay.cost_is_partial("run-1"));
    overlay.apply_event(&completed("unknown"));
    overlay.refresh_costs(&settings());
    assert!((overlay.cost("run-1").expect("cost") - 0.082316).abs() < 1e-10);
    assert!(overlay.cost_is_partial("run-1"));
}

fn completed_for(run_id: &str, model: &str) -> Event {
    let mut event = completed(model);
    if let event_bus::EventKind::Provider(ProviderEvent::RequestCompleted { run_id: run, .. }) =
        &mut event.kind
    {
        *run = Some(run_id.into());
    }
    event
}

fn priced_settings(input_price: f64) -> ProviderSettingsModel {
    let mut config = Config::default();
    let mut entry = ModelEntryConfig::enabled("model");
    entry.input_price = Some(input_price);
    entry.output_price = Some(2.0);
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
fn cached_costs_follow_a_price_edit() {
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed("model"));
    let before = priced_settings(0.5);
    overlay.refresh_costs(&before);
    overlay.refresh_costs(&before);
    assert_eq!(
        overlay.cost("run-1"),
        overlay.estimated_cost("run-1", &before)
    );
    let after = priced_settings(1.0);
    overlay.refresh_costs(&after);
    assert_ne!(
        overlay.cost("run-1"),
        overlay.estimated_cost("run-1", &before)
    );
    assert_eq!(
        overlay.cost("run-1"),
        overlay.estimated_cost("run-1", &after)
    );
}

#[test]
fn cached_costs_add_new_requests_without_touching_other_runs() {
    let settings = priced_settings(0.5);
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed_for("run-1", "model"));
    overlay.refresh_costs(&settings);
    let first = overlay.cost("run-1").expect("first run cost");
    overlay.apply_event(&completed_for("run-2", "model"));
    overlay.apply_event(&completed_for("run-2", "unknown"));
    overlay.refresh_costs(&settings);
    assert_eq!(overlay.cost("run-1"), Some(first));
    assert!(!overlay.cost_is_partial("run-1"));
    assert_eq!(overlay.cost("run-2"), Some(first));
    assert!(overlay.cost_is_partial("run-2"));
    overlay.apply_event(&completed_for("run-1", "model"));
    overlay.refresh_costs(&settings);
    assert!((overlay.cost("run-1").expect("updated cost") - 2.0 * first).abs() < 1e-12);
}

#[tokio::test]
async fn cached_costs_follow_a_replaced_catalog() {
    async fn catalog(input: f64) -> (tempfile::TempDir, Arc<catalog::ModelCatalog>) {
        let dir = tempfile::tempdir().expect("cache directory");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        std::fs::write(dir.path().join("models-dev.json"), format!(r#"{{"fetched_at":{now},"api":{{"vendor":{{"models":{{"model":{{"id":"model","cost":{{"input":{input},"output":2.0}}}}}}}}}}}}"#)).expect("cache fixture");
        let loaded = catalog::ModelCatalog::load_or_refresh(dir.path())
            .await
            .expect("catalog");
        (dir, Arc::new(loaded))
    }
    let mut settings = ProviderSettingsModel::seed_from_config(&Config::default());
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed("model"));
    let (_first_dir, first) = catalog(1.0).await;
    settings.catalog.catalog = Some(first);
    overlay.refresh_costs(&settings);
    let before = overlay.cost("run-1").expect("first catalog cost");
    let (_second_dir, second) = catalog(3.0).await;
    settings.catalog.catalog = Some(second);
    overlay.refresh_costs(&settings);
    let after = overlay.cost("run-1").expect("second catalog cost");
    assert!(after > before);
    assert_eq!(Some(after), overlay.estimated_cost("run-1", &settings));
}

/// A sandbox escalation review billed to the run with a model that has no price.
fn unpriced_review() -> Event {
    let mut event = completed("unknown");
    if let event_bus::EventKind::Provider(ProviderEvent::RequestCompleted { purpose, .. }) =
        &mut event.kind
    {
        *purpose = Some(event_bus::RequestPurpose::EscalationReview);
    }
    event
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
    overlay.apply_event(&unpriced_review());
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
    // The review adds its tokens, and its missing price leaves a lower-bound cost.
    for label in ["worker", "input 196600", "output 30800", "$0.041*"] {
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

fn request(input_tokens: u64, service_tier: Option<event_bus::ServiceTier>) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: format!("req-{input_tokens}-{service_tier:?}"),
        provider: "vendor".into(),
        profile: Some("local".into()),
        protocol: "fixture".into(),
        model: "model".into(),
        streaming: true,
        duration_ms: 0,
        input_tokens,
        output_tokens: 10_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        service_tier,
        run_id: Some("run-1".into()),
        purpose: None,
        reasoning_tokens: None,
    })
}

#[tokio::test]
async fn each_request_is_priced_at_its_own_context_tier_and_speed() {
    // Given: catalog prices of $10/$50 per M, $20/$75 above 272K input and $20/$100 for fast.
    let dir = tempfile::tempdir().expect("cache directory");
    std::fs::write(
        dir.path().join("models-dev.json"),
        serde_json::json!({"fetched_at": 4_102_444_800_u64, "api": {"vendor": {"models": {"model": {
            "id": "model",
            "cost": {"input": 10.0, "output": 50.0,
                "tiers": [{"input": 20.0, "output": 75.0, "tier": {"type": "context", "size": 272_000}}]},
            "experimental": {"modes": {"fast": {"cost": {"input": 20.0, "output": 100.0}}}}
        }}}}})
        .to_string(),
    )
    .expect("cache fixture");
    let mut config = Config::default();
    config.providers.insert(
        "local".into(),
        ProviderProfileConfig {
            models: vec![ModelEntryConfig::enabled("model")],
            ..ProviderProfileConfig::default()
        },
    );
    let mut settings = ProviderSettingsModel::seed_from_config(&config);
    settings.catalog.catalog = Some(Arc::new(
        catalog::ModelCatalog::load_or_refresh(dir.path())
            .await
            .expect("catalog"),
    ));

    // When: a small, a large and a small fast request complete in one run.
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&request(100_000, None));
    overlay.apply_event(&request(300_000, None));
    overlay.apply_event(&request(100_000, Some(event_bus::ServiceTier::Priority)));

    // Then: $1.5 standard + $6.75 at the 272K tier + $3 fast.
    let cost = overlay.estimated_cost("run-1", &settings).expect("cost");
    assert!((cost - 11.25).abs() < 1e-9, "{cost}");
}
