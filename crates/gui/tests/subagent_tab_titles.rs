use egui::epaint::Shape;
use egui_kittest::Harness;
use event_bus::{AgentRunPhase, Event, LifecycleEvent};
use gui::{app::WorkbenchState, fixture::DemoSource};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn state() -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("demo");
    sidebar
        .add_project(project.clone(), "demo", std::path::Path::new("/tmp"))
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("owner"), project.clone(), "fix-login")
        .unwrap();
    sidebar
        .create_thread(ThreadId::new("other"), project, "other-work")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("other")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    state.apply_events([Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "root".into(),
        parent_run_id: None,
        agent_name: "chat:owner".into(),
        role: "orchestrator".into(),
    })]);
    state
}

fn started() -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "worker-1".into(),
        parent_run_id: Some("root".into()),
        agent_name: "worker-1".into(),
        role: "worker".into(),
    })
}

fn completed() -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "worker-1".into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Done,
        reason: None,
    })
}

fn render(state: WorkbenchState<DemoSource>) -> Harness<'static, WorkbenchState<DemoSource>> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1600.0, 900.0))
        .build_ui_state(
            |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
            state,
        );
    harness.run_steps(3);
    harness
}

fn painted_text<'a>(harness: &'a Harness<'_, WorkbenchState<DemoSource>>) -> Vec<&'a str> {
    harness
        .output()
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            Shape::Text(text) => Some(text.galley.text()),
            _ => None,
        })
        .collect()
}

/// Tab titles are painted as `<icon> <title>`.
fn has_tab_title(painted: &[&str], title: &str) -> bool {
    painted.iter().any(|text| {
        text.strip_suffix(title)
            .is_some_and(|icon| icon.ends_with(' '))
    })
}

#[test]
fn started_tab_uses_owner_id_only_when_owner_thread_is_selected() {
    let mut state = state();
    state.apply_events([started()]);

    // A child run belongs to its owner thread, so it must not appear on another thread.
    let mut harness = render(state);
    assert!(
        !painted_text(&harness)
            .iter()
            .any(|title| title.contains("worker-1"))
    );

    harness
        .state_mut()
        .switch_thread(ThreadId::new("owner"))
        .unwrap();
    harness.run_steps(3);
    let text = painted_text(&harness);
    assert!(
        text.iter()
            .any(|title| title.contains("worker-1") && title.contains("owner")),
        "{text:?}"
    );
    assert!(!text.iter().any(|title| {
        title.contains("worker-1") && (title.contains("fix-login") || title.contains("other-work"))
    }));
}

#[test]
fn parked_tab_keeps_owner_and_subagent_count_after_workspace_reload() {
    // Given: a completed child and durable sidebar/workspace state.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("workspace.json");
    let mut state = state().with_save_path(path.clone());
    state.apply_events([started(), completed()]);
    let sidebar = state.sidebar().clone();
    let mut original = render(state);
    original.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::S);
    original.run_steps(3);
    let workspace = workspace_ui::load_from(&path).unwrap();
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(workspace);
    // A new workbench restores the pane for the owner thread without replayed events.
    let mut restored = WorkbenchState::new(DemoSource(Vec::new()), &settings)
        .unwrap()
        .with_sidebar(sidebar);
    restored.switch_thread(ThreadId::new("owner")).unwrap();
    let harness = render(restored);
    // The parked tab carries the owner ID; the region has its run count without an owner suffix.
    let text = painted_text(&harness);
    assert!(
        text.iter()
            .any(|title| title.contains("worker-1") && title.contains("owner")),
        "{text:?}"
    );
    assert!(has_tab_title(&text, "Subagents(1)"));
    assert!(
        !text
            .iter()
            .any(|title| title.contains("Subagents") && title.contains("owner"))
    );
}

#[test]
fn subagent_count_is_live_thread_local_and_retains_completed_nested_runs() {
    let mut harness = render(state());
    assert!(has_tab_title(&painted_text(&harness), "Subagents(0)"));
    harness.state_mut().apply_events([started(), started()]);
    harness.run_steps(3);
    assert!(has_tab_title(&painted_text(&harness), "Subagents(0)"));

    harness
        .state_mut()
        .switch_thread(ThreadId::new("owner"))
        .unwrap();
    harness.run_steps(3);
    assert!(has_tab_title(&painted_text(&harness), "Subagents(1)"));

    harness.state_mut().apply_events([
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "nested-worker".into(),
            parent_run_id: Some("worker-1".into()),
            agent_name: "nested-worker".into(),
            role: "worker".into(),
        }),
        completed(),
    ]);
    harness.run_steps(3);
    assert!(has_tab_title(&painted_text(&harness), "Subagents(2)"));

    harness
        .state_mut()
        .switch_thread(ThreadId::new("other"))
        .unwrap();
    harness.run_steps(3);
    assert!(has_tab_title(&painted_text(&harness), "Subagents(0)"));
}

#[test]
fn completed_subagent_count_survives_history_restore_without_transcript_panels() {
    let mut sidebar = state().sidebar().clone();
    sidebar.switch_thread(&ThreadId::new("owner")).unwrap();
    let events = [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-101".into(),
            parent_run_id: None,
            agent_name: "chat:Worker:owner".into(),
            role: "worker".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-102".into(),
            parent_run_id: Some("run-101".into()),
            agent_name: "reviewer".into(),
            role: "reviewer".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-102".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }),
    ];
    let source = DemoSource(vec![runtime::AgentSummary {
        run_id: runtime::RunId::new(102),
        parent_run_id: Some(runtime::RunId::new(101)),
        name: "reviewer".into(),
        role_name: "reviewer".into(),
        phase: AgentRunPhase::Done,
        model: "demo".into(),
        category: None,
    }]);
    let mut live = WorkbenchState::new(source, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    live.apply_events(events.clone());
    let sidebar = live.sidebar().clone();
    // A live task row and its parked pane represent the same launched agent.
    assert!(has_tab_title(&painted_text(&render(live)), "Subagents(1)"));

    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    for event in &events {
        storage.handle().append_event(Some("gui"), event).unwrap();
    }
    let mut restored = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    restored
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    assert!(has_tab_title(
        &painted_text(&render(restored)),
        "Subagents(1)"
    ));
}
