use event_bus::{AgentRunPhase, Event, LifecycleEvent, ProviderEvent};
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn state(root: &std::path::Path) -> WorkbenchState<DemoSource> {
    std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).expect("config directory");
    std::fs::write(
        config::project_main_config_path(root),
        r#"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
default_model = "base"
models = [{ id = "base", enabled = true, input_price = 1.0, output_price = 2.0 }]
"#,
    )
    .unwrap();
    let config = config::Config::load(&config::LoadOptions {
        project_dir: Some(root.into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["one", "two"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_settings(
            gui::model::provider_settings::ProviderSettingsModel::seed_from_config(&config),
        )
}

fn started(run: &str, agent: &str, parent: Option<&str>) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: agent.into(),
        role: "worker".into(),
    })
}

fn billed(run: &str, tokens: u64) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: format!("request-{run}"),
        provider: "local".into(),
        profile: None,
        protocol: "openai-completions".into(),
        model: "base".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens: tokens,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some(run.into()),
        purpose: None,
        reasoning_tokens: None,
    })
}

fn costs(state: WorkbenchState<DemoSource>) -> Vec<Option<f64>> {
    let mut gui = HeadlessWorkbench::new(state, [1600.0, 1000.0]);
    gui.run();
    gui.state()
        .sidebar()
        .threads
        .iter()
        .map(|thread| gui.state().telemetry().thread_metrics(&thread.run_ids).cost)
        .collect()
}

#[test]
fn metrics_are_independent_when_two_threads_bill_distinct_runs() {
    // Given: two threads with explicitly identified runs.
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.apply_events([
        started("a", "chat:one", None),
        started("b", "chat:two", None),
    ]);
    // When: their distinct usage is billed.
    state.apply_events([billed("a", 1_000_000), billed("b", 2_000_000)]);
    // Then: each aggregate contains only its own cost.
    assert_eq!(costs(state), [Some(1.0), Some(2.0)]);
}

#[test]
fn orphan_metrics_stay_unassigned_when_active_thread_changes() {
    // Given: a switch after two independently billed conversations.
    for parent in [None, Some("missing-parent")] {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        state.apply_events([
            started("a", "chat:one", None),
            billed("a", 1_000_000),
            started("b", "chat:two", None),
            billed("b", 2_000_000),
        ]);
        state.switch_thread(ThreadId::new("two")).unwrap();
        // When: an unowned background run starts and bills after the switch.
        state.apply_events([
            started("background", "worker", parent),
            billed("background", 9_000_000),
        ]);
        // Then: neither conversation claims the unrelated cost.
        assert_eq!(costs(state), [Some(1.0), Some(2.0)], "parent={parent:?}");
    }
}

#[test]
fn child_metrics_follow_parent_when_another_thread_is_active() {
    // Given: the parent belongs to the now-inactive conversation.
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.apply_events([started("a", "chat:one", None), billed("a", 1_000_000)]);
    state.switch_thread(ThreadId::new("two")).unwrap();
    // When: its child bills in the background.
    state.apply_events([
        started("child", "worker", Some("a")),
        billed("child", 3_000_000),
    ]);
    // Then: the parent thread, not the active thread, receives the cost.
    assert_eq!(costs(state), [Some(4.0), None]);
}

#[test]
fn metrics_render_in_status_line_below_composer() {
    // Given: a completed conversation with known cost.
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.apply_events([
        started("a", "chat:one", None),
        billed("a", 1_000_000),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "a".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }),
    ]);
    let mut gui = HeadlessWorkbench::new(state, [1600.0, 1000.0]);
    // When: the real workbench renders.
    gui.run();
    // Then: cost lives in the status line below the composer; header keeps only the title.
    let title = gui.label_rects("Thread: one")[0];
    let metrics = gui.label_rects("$1.000");
    assert_eq!(metrics.len(), 1);
    let metrics = metrics[0];
    let composer = gui.label_rects("Message or /command")[0];
    assert!(
        metrics.min.y > composer.max.y && title.max.y < composer.min.y,
        "title={title:?}, composer={composer:?}, metrics={metrics:?}"
    );
}

#[test]
fn conversation_status_excludes_child_cost_while_header_shows_thread_total() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.apply_events([
        started("a", "chat:one", None),
        billed("a", 1_000_000),
        started("child", "worker", Some("a")),
        billed("child", 3_000_000),
    ]);
    let mut gui = HeadlessWorkbench::new(state, [1600.0, 1000.0]);
    gui.run();
    let runs = &gui.state().sidebar().threads[0].run_ids;
    let metrics = gui.state().telemetry().thread_metrics(runs);
    assert_eq!(metrics.conversation_cost, Some(1.0));
    assert_eq!(metrics.cost, Some(4.0));
    let title = gui.label_rects("Thread: one")[0];
    let total = gui.label_rects("Total cost $4.000")[0];
    let main = gui.label_rects("$1.000")[0];
    let composer = gui.label_rects("Message or /command")[0];
    assert!(total.top() >= title.bottom());
    assert!(total.bottom() < composer.top());
    assert!(main.top() > composer.bottom());
}

#[test]
fn conversation_and_focused_agent_show_their_own_request_metrics() {
    for focused_agent in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        if focused_agent {
            // Select the conversation focus before automatic child tabs exist.
            state.drill_down("child");
        }
        state.apply_events([
            started("a", "chat:one", None),
            started("child", "worker", Some("a")),
        ]);
        for (run, output, ttft) in [("a", 40, 200), ("child", 400, 900)] {
            state.apply_events([Event::new(ProviderEvent::FirstTokenObserved {
                request_id: format!("request-{run}"),
                provider: "local".into(),
                profile: None,
                protocol: "openai-completions".into(),
                model: "base".into(),
                ttft_ms: ttft,
                run_id: Some(run.into()),
            })]);
            let mut completion = billed(run, 1_000);
            if let event_bus::EventKind::Provider(ProviderEvent::RequestCompleted {
                duration_ms,
                output_tokens,
                ..
            }) = &mut completion.kind
            {
                *duration_ms = 2_000;
                *output_tokens = output;
            }
            state.apply_events([completion]);
        }
        let mut gui = HeadlessWorkbench::new(state, [1600.0, 1000.0]);
        gui.run();
        let (ttft, rate) = if focused_agent {
            ("TTFT 900ms (avg 900ms)", "200.0 tok/s (avg 200.0 tok/s)")
        } else {
            ("TTFT 200ms (avg 200ms)", "20.0 tok/s (avg 20.0 tok/s)")
        };
        assert!(gui.has_label(ttft), "missing {ttft}");
        assert!(gui.has_label(rate), "missing {rate}");
        let own_cost = if focused_agent { "$0.002" } else { "$0.001" };
        assert!(gui.has_label(own_cost), "missing {own_cost}");
    }
}
