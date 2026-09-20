use config::{Config, ModelEntryConfig, ProviderProfileConfig};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, ProviderEvent};
use gui::model::{provider_settings::ProviderSettingsModel, telemetry::TelemetryOverlay};
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn settings(window: Option<u64>) -> ProviderSettingsModel {
    let mut config = Config::default();
    let mut entry = ModelEntryConfig::enabled("model");
    entry.context_window = window;
    config.providers.insert(
        "local".into(),
        ProviderProfileConfig {
            models: vec![entry],
            ..Default::default()
        },
    );
    ProviderSettingsModel::seed_from_config(&config)
}

fn completed(input: u64) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: format!("request-{input}"),
        provider: "vendor".into(),
        profile: Some("local".into()),
        protocol: "fixture".into(),
        model: "model".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens: input,
        output_tokens: 900,
        cache_read_tokens: 200,
        cache_write_tokens: 100,
        finish_reason: "stop".into(),
        run_id: Some("run-1".into()),
    })
}

#[test]
fn pressure_uses_latest_input_side_tokens_when_requests_accumulate() {
    // Given: an earlier request whose usage must not enter the pressure numerator.
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed(800));
    // When: the latest request occupies 100 + 200 + 100 of 1000 tokens.
    overlay.apply_event(&completed(100));
    overlay.refresh_costs(&settings(Some(1000)));
    // Then: output tokens and previous requests are excluded.
    assert_eq!(
        overlay
            .row("run-1")
            .unwrap()
            .context_pressure_label()
            .as_deref(),
        Some("40%")
    );
}

#[test]
fn pressure_is_omitted_when_window_is_unknown_or_zero() {
    for window in [None, Some(0)] {
        // Given: completed usage with an unusable context window.
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&completed(100));
        // When: settings are resolved.
        overlay.refresh_costs(&settings(window));
        // Then: no misleading percentage is rendered.
        assert_eq!(overlay.row("run-1").unwrap().context_pressure_label(), None);
    }
}

#[test]
fn pressure_is_omitted_when_next_request_has_no_usage_yet() {
    // Given: pressure from a completed request.
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed(100));
    overlay.refresh_costs(&settings(Some(1000)));
    // When: the next request switches models but has not reported usage.
    overlay.apply_event(&Event::new(ProviderEvent::RequestStarted {
        request_id: "next".into(),
        provider: "vendor".into(),
        profile: Some("local".into()),
        protocol: "fixture".into(),
        model: "other".into(),
        streaming: true,
        run_id: Some("run-1".into()),
    }));
    overlay.refresh_costs(&settings(Some(1000)));
    // Then: the old context is not attributed to the new model.
    assert_eq!(overlay.row("run-1").unwrap().context_pressure_label(), None);
}

#[test]
fn pressure_uses_preset_when_manual_window_is_absent() {
    // Given: a configured preset instead of a manual window.
    let mut config = Config::default();
    let mut entry = ModelEntryConfig::enabled("model");
    entry.preset = Some("large".into());
    config.model_presets.insert(
        "large".into(),
        config::ModelPresetConfig {
            context_window: Some(2000),
            ..Default::default()
        },
    );
    config.providers.insert(
        "local".into(),
        ProviderProfileConfig {
            models: vec![entry],
            ..Default::default()
        },
    );
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed(100));
    // When: resolving the request's profile alias through shared metadata.
    overlay.refresh_costs(&ProviderSettingsModel::seed_from_config(&config));
    // Then: 400 / 2000 is 20%, not the manual-fixture's 40%.
    assert_eq!(
        overlay
            .row("run-1")
            .unwrap()
            .context_pressure_label()
            .as_deref(),
        Some("20%")
    );
}

#[test]
fn pressure_handles_large_counts_without_overflow_or_clamping() {
    // Given: a request whose input-side sum exceeds u64::MAX.
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed(u64::MAX));
    // When: resolving against the largest supported window.
    overlay.refresh_costs(&settings(Some(u64::MAX)));
    // Then: rounding is stable without overflowing input-side addition.
    assert_eq!(
        overlay
            .row("run-1")
            .unwrap()
            .context_pressure_label()
            .as_deref(),
        Some("100%")
    );
}

#[test]
fn thread_pressure_uses_latest_request_instead_of_run_list_order() {
    // Given: the second run has a later request than the first.
    let mut overlay = TelemetryOverlay::new();
    let mut second = completed(600);
    if let event_bus::EventKind::Provider(ProviderEvent::RequestCompleted { run_id, .. }) =
        &mut second.kind
    {
        *run_id = Some("run-2".into());
    }
    overlay.apply_event(&completed(100));
    overlay.apply_event(&second);
    overlay.refresh_costs(&settings(Some(1000)));
    // When: aggregating a thread with reverse run order.
    let metrics = overlay.thread_metrics(&["run-2".into(), "run-1".into()]);
    // Then: pressure is neither summed nor selected by run-list order.
    assert_eq!(metrics.context_pressure, Some(90));
}

#[test]
fn thread_pressure_excludes_runs_owned_by_other_threads() {
    // Given: one billed run exists outside this thread.
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&completed(100));
    overlay.refresh_costs(&settings(Some(1000)));
    // When: rendering an unrelated thread's metrics.
    let metrics = overlay.thread_metrics(&["other-run".into()]);
    // Then: no global context value leaks into this thread.
    assert_eq!(metrics.context_pressure, None);
}

fn workbench(window: Option<u64>) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", &std::env::temp_dir())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("one"), project, "one")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    let source = DemoSource(vec![runtime::AgentSummary {
        run_id: runtime::RunId::new(1),
        parent_run_id: Some(runtime::RunId::new(0)),
        name: "chat:one".into(),
        role_name: "worker".into(),
        phase: AgentRunPhase::Running,
        model: "irrelevant-task-model".into(),
    }]);
    let mut state = WorkbenchState::new(source, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_settings(settings(window));
    state.apply_events([
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-1".into(),
            parent_run_id: None,
            agent_name: "chat:one".into(),
            role: "worker".into(),
        }),
        completed(800),
        completed(100),
    ]);
    HeadlessWorkbench::new(state, [1600.0, 1000.0])
}

#[test]
fn agents_cell_shows_pressure_when_window_is_known() {
    // Given: two requests, a profile alias, and a known window.
    let mut gui = workbench(Some(1000));
    // When: the real workbench renders its Agents pane.
    gui.run();
    // Then: cumulative in/out remains intact beside latest-request pressure.
    assert!(gui.has_label("900 / 1800 (40%)"));
    if let Some(path) = std::env::var_os("CONTEXT_AGENTS_CAPTURE") {
        let mut tasks = gui::model::tasks::TasksModel::new(DemoSource(vec![]));
        tasks.update(&[runtime::AgentSummary {
            run_id: runtime::RunId::new(1),
            parent_run_id: Some(runtime::RunId::new(0)),
            name: "worker".into(),
            role_name: "worker".into(),
            phase: AgentRunPhase::Done,
            model: "model".into(),
        }]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 220.0))
            .build_ui(|ui| {
                gui::theme::install(ui.ctx());
                gui::panes::agents::agents_pane(ui, &tasks, gui.state().telemetry());
            });
        harness.run();
        harness
            .render()
            .unwrap()
            .save(std::path::Path::new(&path))
            .unwrap();
    }
}

#[test]
fn conversation_status_line_shows_context_when_window_is_known() {
    // Given: a thread owns the run with 40% pressure.
    let mut gui = workbench(Some(1000));
    // When: the conversation renders.
    gui.run();
    // Then: context lives in the status line below the composer, not the header.
    let status = gui.label_rects("ctx 40%")[0];
    let composer = gui.label_rects("Message or /command")[0];
    assert!(
        status.min.y > composer.max.y,
        "status={status:?} composer={composer:?}"
    );
    assert!(gui.label_rects("ctx 40%").len() == 1);
    if let Some(path) = std::env::var_os("CONTEXT_PRESSURE_CAPTURE") {
        gui.capture()
            .unwrap()
            .save_png(std::path::Path::new(&path))
            .unwrap();
    }
}

#[test]
fn agents_cell_preserves_tokens_when_window_is_unknown() {
    // Given: no metadata for this model.
    let mut gui = workbench(None);
    // When: the workbench renders.
    gui.run();
    // Then: the existing token display remains available without a percentage.
    assert!(gui.has_label("900 / 1800"));
}
