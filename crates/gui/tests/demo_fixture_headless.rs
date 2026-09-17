use gui::app::WorkbenchState;
use gui::fixture::{DemoSource, demo_runs, demo_sidebar, populate};
use gui::headless::HeadlessWorkbench;
use workspace_ui::{PanelId, ThreadRunPhase, UiSettings};

#[test]
fn demo_fixture_populates_sidebar_conversation_and_agents() {
    // Given: a deterministic demo fixture state.
    let dir = tempfile::tempdir().expect("temp dir");
    let sidebar = demo_sidebar(dir.path()).expect("demo sidebar");
    let state = populate(
        WorkbenchState::new(DemoSource(demo_runs()), &UiSettings::default())
            .expect("default state builds"),
        sidebar,
    );
    let mut workbench = HeadlessWorkbench::new(state, [1280.0, 720.0]);
    workbench.run();

    // Then: the selected project's threads, thread transcript, and merge state render.
    assert!(workbench.has_label("Thread: Refine GUI design system"));
    assert!(!workbench.has_label("Queue seed CLI"));
    assert!(
        workbench.has_label("Analysing t3code design language and mapping tokens to egui Visuals…")
    );
    assert_eq!(
        workbench
            .state()
            .merge()
            .view
            .pr
            .as_ref()
            .map(|pr| pr.number),
        Some(81)
    );
    assert_eq!(
        workbench.state().thread_phases().get("run-1"),
        Some(&ThreadRunPhase::Running)
    );
    assert_eq!(
        workbench.state().sidebar().threads[0].run_ids,
        ["run-1", "run-2", "run-3"]
    );
    assert_eq!(workbench.state().tasks().rows().len(), 3);
}

#[test]
fn active_demo_thread_displays_root_message_and_tool() {
    // Given: the same fixture construction used by the demo binary.
    let dir = tempfile::tempdir().unwrap();
    let sidebar = demo_sidebar(dir.path()).unwrap();
    assert!(
        sidebar
            .threads
            .iter()
            .all(|thread| thread.run_ids.is_empty()),
        "demo ownership must be established by production event binding"
    );
    let mut state = populate(
        WorkbenchState::new(DemoSource(demo_runs()), &UiSettings::default()).unwrap(),
        sidebar,
    );
    state
        .switch_thread(workspace_ui::ThreadId::new("thread-2"))
        .unwrap();
    state
        .switch_thread(workspace_ui::ThreadId::new("thread-1"))
        .unwrap();
    let mut workbench = HeadlessWorkbench::new(state, [1600.0, 1000.0]);
    // When: the active conversation renders (without opening an agent pane).
    workbench.run();
    // Then: run-1's message and tool are both in that conversation.
    assert!(
        workbench.has_label("Analysing t3code design language and mapping tokens to egui Visuals…")
    );
    assert!(workbench.has_label("read_file"));
}

#[test]
fn goal_binding_is_restored_when_sidebar_has_no_run_ids() {
    // Given: durable goal events without pre-seeded sidebar ownership.
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    for event in gui::fixture::demo_events() {
        storage.handle().append_event(Some("demo"), &event).unwrap();
    }
    storage.close();
    let mut state = WorkbenchState::new(DemoSource(demo_runs()), &UiSettings::default())
        .unwrap()
        .with_sidebar(demo_sidebar(dir.path()).unwrap());
    // When: a new GUI replays the persisted events.
    state
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    let mut workbench = HeadlessWorkbench::new(state, [1600.0, 1000.0]);
    workbench.run();
    // Then: root/children ownership and the active transcript are reconstructed.
    assert_eq!(
        workbench.state().sidebar().threads[0].run_ids,
        ["run-1", "run-2", "run-3"]
    );
    assert!(
        workbench.has_label("Analysing t3code design language and mapping tokens to egui Visuals…")
    );
    assert!(
        workbench
            .state()
            .transcript()
            .entries()
            .iter()
            .any(|entry| matches!(
                entry,
                gui::model::transcript::TranscriptEntry::Tool { tool_name, .. }
                    if tool_name == "read_file"
            ))
    );
}

#[test]
fn demo_sidebar_rejects_missing_root() {
    // Given: a root path that does not exist on disk.
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("missing-root");

    // When: the demo sidebar is built from that root.
    let error = demo_sidebar(&missing).expect_err("missing root must fail");

    // Then: the failure is the typed missing-root error.
    assert!(matches!(error, gui::fixture::FixtureError::MissingRoot(_)));
}

#[test]
fn demo_state_has_no_duplicate_interactive_labels() {
    // Given: labels whose duplication would panic kittest single-node queries.
    const LABELS: &[&str] = &[
        "Add project",
        "New thread",
        "Go to Projects",
        "Start a thread",
        "Open default panes",
        "Working tree",
        "Branch vs main",
        "← Thread",
        "Projects",
        "Conversation",
        "Agents",
    ];
    const RIGHT_TABS: &[&str] = &["agents-main", "diff-main", "terminal-main"];

    let dir = tempfile::tempdir().expect("temp dir");
    let demo = populate(
        WorkbenchState::new(DemoSource(demo_runs()), &UiSettings::default())
            .expect("default state builds"),
        demo_sidebar(dir.path()).expect("demo sidebar"),
    );
    let empty = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds");

    // When: each state renders with every right-area tab active in turn.
    for state in [empty, demo] {
        let mut workbench = HeadlessWorkbench::new(state, [1280.0, 720.0]);
        workbench.run();
        assert_unique_labels(&workbench, LABELS);
        for tab in RIGHT_TABS {
            activate_tab(&mut workbench, tab);
            workbench.run();
            // Then: no label binds to two or more nodes in any frame.
            assert_unique_labels(&workbench, LABELS);
        }
    }
}

fn assert_unique_labels(workbench: &HeadlessWorkbench<DemoSource>, labels: &[&str]) {
    for label in labels {
        let count = workbench.count_labels(label);
        assert!(
            count <= 1,
            "label {label:?} matched {count} nodes in one frame"
        );
    }
}

fn activate_tab(workbench: &mut HeadlessWorkbench<DemoSource>, panel_id: &str) {
    let path = workbench
        .state()
        .dock()
        .find_tab(&PanelId::new(panel_id))
        .expect("panel tab exists");
    workbench
        .state_mut()
        .dock_mut()
        .set_active_tab(path)
        .expect("activate tab");
}
