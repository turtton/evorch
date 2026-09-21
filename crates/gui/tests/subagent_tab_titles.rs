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

#[test]
fn started_tab_shows_owner_not_active_thread() {
    // Given: another thread is selected.
    let mut state = state();
    // When: the owner's child starts and the actual dock is rendered.
    state.apply_events([started()]);
    let harness = render(state);
    // Then: the tab identifies its owner, not the selected thread.
    let text = painted_text(&harness);
    assert!(
        text.iter()
            .any(|title| title.contains("worker-1") && title.contains("thread: fix-login")),
        "{text:?}"
    );
    assert!(
        !text
            .iter()
            .any(|title| title.contains("worker-1") && title.contains("other-work"))
    );
}

#[test]
fn parked_tab_keeps_owner_but_placeholder_has_no_badge_after_reload() {
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
    // When: a new workbench renders the persisted pane without replayed events.
    let restored = WorkbenchState::new(DemoSource(Vec::new()), &settings)
        .unwrap()
        .with_sidebar(sidebar);
    let harness = render(restored);
    // Then: the parked tab retains attribution and the region stays global.
    let text = painted_text(&harness);
    assert!(
        text.iter()
            .any(|title| title.contains("worker-1") && title.contains("thread: fix-login")),
        "{text:?}"
    );
    assert!(text.contains(&"Subagents"));
    assert!(
        !text
            .iter()
            .any(|title| title.contains("Subagents") && title.contains("thread:"))
    );
}
