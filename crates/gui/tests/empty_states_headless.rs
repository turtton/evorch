use gui::app::{ConversationFocus, WorkbenchState};
use gui::fixture::{DemoSource, demo_events, demo_runs, demo_sidebar};
use gui::headless::HeadlessWorkbench;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

fn is_active_tab(dock: &egui_dock::DockState<PanelId>, panel_id: &PanelId) -> bool {
    let Some(tab_path) = dock.find_tab(panel_id) else {
        return false;
    };
    let Ok(leaf) = dock.leaf(tab_path.node_path()) else {
        return false;
    };
    leaf.active.0 == tab_path.tab.0
}

fn sidebar_with_project(root: &std::path::Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project can be added");
    sidebar
        .select_project(&project_id)
        .expect("project can be selected");
    sidebar
}

#[test]
fn conversation_without_project_offers_go_to_projects() {
    let workbench = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds");
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);
    harness.run();

    assert!(harness.has_label("No project selected"));
    harness.click_label("Go to Projects");
    harness.run();
    assert!(is_active_tab(
        harness.state().dock(),
        &PanelId::new("sidebar-main")
    ));
}

#[test]
fn conversation_with_project_but_no_thread_offers_start_thread() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = sidebar_with_project(temp.path());
    let workbench = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar);
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);
    harness.run();

    assert!(harness.has_label("No thread selected"));
    harness.click_label("Start a thread");
    harness.run();
    assert!(harness.state().sidebar().active_thread.is_some());
    assert!(harness.has_label("No messages yet"));
    assert!(harness.has_label("Type a message below, or /goal <text> to start the loop."));
}

#[test]
fn conversation_with_messages_hides_placeholders() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = demo_sidebar(temp.path()).expect("demo sidebar builds");
    let workbench = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar);
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);
    harness.run();
    harness.state_mut().apply_events(demo_events());
    harness.run();

    assert!(!harness.has_label("No project selected"));
    assert!(!harness.has_label("No thread selected"));
    assert!(!harness.has_label("No messages yet"));
    assert!(harness.has_label(
        "Message: Analysing t3code design language and mapping tokens to egui Visuals…"
    ));
}

#[test]
fn agent_focus_header_keeps_identity_label_and_return_button() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = demo_sidebar(temp.path()).expect("demo sidebar builds");
    let workbench = WorkbenchState::new(DemoSource(demo_runs()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar);
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);
    harness.run();
    harness.state_mut().apply_events(demo_events());
    harness.run();

    harness.click_label("run-2");
    harness.run();
    assert!(harness.has_label("run-2 / implementer / worker"));

    harness.click_label("← Thread");
    harness.run();
    assert_eq!(harness.state().focus(), &ConversationFocus::Thread);
}

#[test]
#[ignore = "writes CJK PNG review evidence using an offscreen GPU adapter"]
fn capture_cjk_conversation_png_evidence() {
    // Given: a Japanese-named project and thread with a Japanese message.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("evorch-日本語");
    sidebar
        .add_project(project_id.clone(), "evorch-日本語", temp.path())
        .expect("project added");
    sidebar
        .select_project(&project_id)
        .expect("project selected");
    sidebar
        .create_thread(
            ThreadId::new("thread-jp"),
            project_id,
            "コンポーザー検証スレッド",
        )
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-jp"))
        .expect("thread selected");
    let workbench = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar);
    let mut harness = HeadlessWorkbench::new(workbench, [1200.0, 900.0]);
    harness.run();
    // Then: the Japanese names render without tofu or clipping and the composer stays docked.
    assert!(harness.has_label("evorch-日本語"));
    assert!(harness.has_label("コンポーザー検証スレッド"));
    let frame = harness.capture().expect("offscreen adapter");
    frame
        .save_png(std::path::Path::new("/tmp/opencode/w-cjk.png"))
        .expect("png saved");
}

#[test]
fn composer_is_docked_at_bottom_in_empty_state() {
    // Given
    let temp = tempfile::tempdir().expect("temp dir");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("state")
        .with_sidebar(demo_sidebar(temp.path()).expect("sidebar"));
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    // When
    harness.run();
    // Then
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new("agent-main"))
        .expect("conversation");
    let bottom = harness
        .state()
        .dock()
        .leaf(path.node_path())
        .expect("leaf")
        .rect
        .max
        .y;
    let send = harness.label_rects("Send")[0];
    assert!(
        send.max.y >= bottom - gui::theme::tokens::SP_4 - gui::theme::tokens::SP_3 - 1.0,
        "send={send:?}, bottom={bottom}"
    );
    assert!(harness.label_rects("No messages yet")[0].center().y < send.min.y);
}

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_empty_composer_evidence() {
    // Given
    let temp = tempfile::tempdir().expect("temp dir");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("state")
        .with_sidebar(demo_sidebar(temp.path()).expect("sidebar"));
    let mut harness = HeadlessWorkbench::new(state, [1280.0, 720.0]);
    // When
    harness.run();
    // Then
    assert!(harness.has_label("No messages yet"));
    harness
        .capture()
        .expect("capture")
        .save_png(std::path::Path::new("/tmp/opencode/w-d-empty.png"))
        .expect("PNG saved");
}
