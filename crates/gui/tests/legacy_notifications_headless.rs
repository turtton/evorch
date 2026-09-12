use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, LifecycleEvent};
use gui::{app::WorkbenchState, fixture::DemoSource};
use workspace_ui::{LayoutNode, PanelId, UiSettings, Workspace, load_settings, save_settings};

#[test]
fn saved_legacy_layout_exposes_notification_row_and_opens_run_transcript() {
    // Given: an actual saved settings file from before notifications were registered.
    let mut legacy = Workspace::default();
    legacy.panels.remove(&PanelId::new("notifications-main"));
    let LayoutNode::Split(root) = &mut legacy.main.root else {
        panic!("split")
    };
    let LayoutNode::Split(content) = root.second.as_mut() else {
        panic!("split")
    };
    let LayoutNode::Tabs(tabs) = content.second.as_mut() else {
        panic!("tabs")
    };
    tabs.panels.retain(|id| id.as_str() != "notifications-main");
    tabs.active = 1;
    assert_notification_opens_transcript(legacy);
}

#[test]
fn notification_opens_transcript_when_saved_layout_has_only_tasks_and_terminal() {
    // Given: a valid saved layout without either fixed transcript anchor.
    let legacy = tasks_and_terminal();
    assert_notification_opens_transcript(legacy);
}

#[test]
fn transcript_opens_in_first_leaf_when_all_fixed_anchors_are_absent() {
    // Given: a layout with no Agents, Agent, or Notifications tab.
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(tasks_and_terminal());
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &settings).unwrap();
    // When: a transcript is opened directly.
    state.open_agent_pane("fallback-run");
    // Then: it is appended and selected beside the existing tabs.
    let dock = state.dock();
    let path = dock
        .find_tab(&PanelId::new("agent-fallback-run"))
        .expect("transcript opened");
    let leaf = dock.leaf(path.node_path()).unwrap();
    assert_eq!(leaf.active, path.tab);
    assert_eq!(
        leaf.tabs,
        vec![
            PanelId::new("tasks-main"),
            PanelId::new("terminal-main"),
            PanelId::new("agent-fallback-run")
        ]
    );
}

fn tasks_and_terminal() -> Workspace {
    let mut workspace = Workspace::default_v01();
    workspace.version = workspace_ui::WORKSPACE_SCHEMA_VERSION;
    workspace.panels.remove(&PanelId::new("agent-main"));
    workspace.main.root = LayoutNode::Tabs(workspace_ui::Tabs {
        panels: vec![PanelId::new("tasks-main"), PanelId::new("terminal-main")],
        active: 1,
    });
    workspace
}

fn assert_notification_opens_transcript(legacy: Workspace) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("ui.toml");
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(legacy);
    save_settings(&settings, &path).unwrap();
    let loaded = load_settings(&path).unwrap();
    let state = WorkbenchState::new(DemoSource(Vec::new()), &loaded).unwrap();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_ui_state(
            |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
            state,
        );
    // When: completion arrives and the user opens Notifications and activates its row.
    harness
        .state_mut()
        .apply_events([Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "legacy-run".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        })]);
    harness.run_steps(3);
    assert_eq!(harness.state().notifications().unread_count(), 1);
    let notification_tab = harness
        .state()
        .dock()
        .find_tab(&PanelId::new("notifications-main"))
        .expect("saved layouts expose the Notifications tab");
    harness
        .state_mut()
        .dock_mut()
        .set_active_tab(notification_tab)
        .unwrap();
    harness.run_steps(3);
    harness.get_by_label("Run legacy-run completed").click();
    harness.run_steps(3);
    // Then: the rendered row opens and selects the targeted run transcript.
    let dock = harness.state().dock();
    let transcript = dock
        .find_tab(&PanelId::new("agent-legacy-run"))
        .expect("transcript opened");
    assert_eq!(
        dock.leaf(transcript.node_path()).unwrap().active,
        transcript.tab
    );
}
