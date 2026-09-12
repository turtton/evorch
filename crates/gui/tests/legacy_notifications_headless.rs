use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, LifecycleEvent};
use gui::{app::WorkbenchState, fixture::DemoSource};
use workspace_ui::{LayoutNode, PanelId, UiSettings, Workspace, load_settings, save_settings};

#[test]
fn saved_legacy_layout_exposes_notification_row_and_opens_run_transcript() {
    // Given: an actual saved settings file from before notifications were registered.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("ui.toml");
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
