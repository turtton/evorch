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
    assert!(harness.has_label("Add a repository in the Projects panel to begin."));
    assert!(harness.has_label("No projects yet"));
    assert!(harness.has_label("Add a repository root to start orchestrating."));
    assert!(harness.has_label("Add project"));
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
    assert!(harness.has_label("Start a thread to open a conversation."));
    assert!(harness.has_label("Start a thread to begin a conversation."));
    assert!(harness.has_label("New thread"));
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
    assert!(
        harness.has_label("Analysing t3code design language and mapping tokens to egui Visuals…")
    );
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
    harness.click_label("run-2");
    harness.run();
    assert_eq!(
        harness.state().focus(),
        &ConversationFocus::Agent("run-2".into())
    );
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
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
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
    let wall = harness.label_rects("wall 0s")[0];
    assert!(
        wall.max.y >= bottom - gui::theme::tokens::SP_4 - gui::theme::tokens::SP_3 - 1.0,
        "status={wall:?}, bottom={bottom}"
    );
    assert!(
        send.max.y < wall.max.y,
        "status line must sit below the composer"
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
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    frame
        .save_png(std::path::Path::new("/tmp/opencode/w-d-empty.png"))
        .expect("PNG saved");
}

#[test]
fn empty_monitoring_panes_explain_what_will_appear() {
    for (panel, title, hint, action) in [
        (
            "agents-main",
            "No agent runs yet",
            "Send a message or /goal in Conversation to start an agent run.",
            Some("Open default panes"),
        ),
        (
            "notifications-main",
            "No notifications yet",
            "Run completions, failures and approval requests will appear here.",
            None,
        ),
        (
            "diff-main",
            "No diff loaded",
            "Choose Working tree or Branch vs main.",
            Some("Working tree"),
        ),
    ] {
        // Given: an empty workbench with the audited pane active.
        let mut state =
            WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("state");
        let path = state.dock().find_tab(&PanelId::new(panel)).expect("tab");
        state.dock_mut().set_active_tab(path).expect("activate");
        let mut harness = HeadlessWorkbench::new(state, [1280.0, 720.0]);
        // When: the pane is displayed.
        harness.run();
        // Then: guidance and any available action are visible.
        assert!(harness.has_label(title), "{panel}: {title}");
        assert!(harness.has_label(hint), "{panel}: {hint}");
        if let Some(action) = action {
            assert!(harness.has_label(action));
        }
    }
}

#[test]
fn empty_agent_transcript_explains_event_delivery() {
    use egui_kittest::{Harness, kittest::Queryable};
    // Given: either an absent transcript or one without events.
    let model = gui::model::transcript::TranscriptModel::default();
    for transcript in [None, Some(&model)] {
        // When: the individual agent pane is rendered.
        let harness = Harness::builder().build_ui(|ui| {
            gui::panes::agent_transcript::agent_transcript_pane(ui, "run-1", transcript);
        });
        // Then: run identity and event-delivery guidance are exposed.
        assert!(harness.query_by_label("no events for run-1").is_some());
        assert!(
            harness
                .query_by_label("Messages and tool activity will appear as this agent runs.")
                .is_some()
        );
    }
}

#[test]
fn empty_diff_keeps_refresh_action_with_guidance() {
    use egui_kittest::{Harness, kittest::Queryable};
    // Given: a successfully fetched empty diff.
    let mut diff = gui::diff::DiffModel::new();
    diff.show_snapshot(String::new());
    // When: rendering the diff pane.
    let harness = Harness::builder().build_ui(|ui| {
        gui::panes::diff::diff_pane(ui, &diff);
    });
    // Then: absence of changes is distinguished from an unfetched diff, with refresh controls.
    assert!(harness.query_by_label("no changes").is_some());
    assert!(
        harness
            .query_by_label("Edit files, then choose Working tree or Branch vs main to refresh.")
            .is_some()
    );
    assert!(harness.query_by_label("Working tree").is_some());
    assert!(harness.query_by_label("Branch vs main").is_some());
}
