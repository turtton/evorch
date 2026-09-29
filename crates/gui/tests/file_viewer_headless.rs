use std::time::{Duration, Instant};

use event_bus::{Event, LifecycleEvent, MessageEvent};
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

fn workbench(root: &std::path::Path, message: &str) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    let thread = ThreadId::new("thread");
    sidebar
        .create_thread(thread.clone(), project, "File links")
        .unwrap();
    sidebar.switch_thread(&thread).unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    state.apply_events([
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-1".into(),
            parent_run_id: None,
            agent_name: "chat:thread".into(),
            role: "worker".into(),
        }),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-1".into()),
            delta: message.into(),
        }),
    ]);
    let mut harness = HeadlessWorkbench::new(state, [1440.0, 900.0]);
    harness.run();
    harness
}

fn wait_label(harness: &mut HeadlessWorkbench<DemoSource>, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        harness.run();
        if harness.has_label(label) {
            return;
        }
        assert!(Instant::now() < deadline, "missing {label}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn conversation_file_links_open_reuse_and_reopen_dock_tabs_at_the_requested_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("source.rs");
    std::fs::write(
        &file,
        (1..=200)
            .map(|line| format!("// line {line}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let mut harness = workbench(
        dir.path(),
        "[Open source](source.rs:120)\n\n[Open again](./source.rs#L150)",
    );
    harness.click_label("Open source");
    wait_label(&mut harness, "// line 120");
    let id = PanelId::new(format!("file-{}", file.display()));
    assert!(harness.state().dock().find_tab(&id).is_some());
    harness.click_label("Open again");
    wait_label(&mut harness, "// line 150");
    assert_eq!(
        harness
            .state()
            .dock()
            .iter_all_tabs()
            .filter(|(_, tab)| *tab == &id)
            .count(),
        1
    );
    let path = harness.state().dock().find_tab(&id).unwrap();
    harness.state_mut().dock_mut().remove_tab(path).unwrap();
    harness.run();
    harness.click_label("Open source");
    wait_label(&mut harness, "// line 120");
    assert!(harness.state().dock().find_tab(&id).is_some());
}

#[test]
fn markdown_links_resolve_against_the_document_directory_and_errors_stay_in_the_viewer() {
    let dir = tempfile::tempdir().unwrap();
    let docs = dir.path().join("docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(
        docs.join("readme.md"),
        "# File preview\n\n[Related source](nearby.rs)\n\n[Missing file](missing.txt)",
    )
    .unwrap();
    std::fs::write(docs.join("nearby.rs"), "fn nearby() {}\n").unwrap();
    let mut harness = workbench(dir.path(), "[Open document](docs/readme.md)");
    harness.click_label("Open document");
    wait_label(&mut harness, "File preview");
    harness.click_label("Related source");
    wait_label(&mut harness, "fn nearby() {}");
    assert!(
        harness
            .state()
            .dock()
            .find_tab(&PanelId::new(format!(
                "file-{}",
                docs.join("nearby.rs").display()
            )))
            .is_some()
    );
    harness.click_label("Open document");
    harness.run();
    harness.click_label("Missing file");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        harness.run();
        if !harness.has_label("ファイルを読み込み中…") {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        harness
            .state()
            .dock()
            .find_tab(&PanelId::new(format!(
                "file-{}",
                docs.join("missing.txt").display()
            )))
            .is_some()
    );
    assert!(!harness.has_label("fn nearby() {}"));
}
