use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use runtime::{AgentInspection, AgentSummary, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRecord, UiSettings};

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }

    fn inspect(&self, _: RunId) -> Option<AgentInspection> {
        None
    }
}

fn fixture(root: &std::path::Path) -> HeadlessWorkbench<EmptySource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for (id, parent, title) in [
        ("root", None, "Parent"),
        ("child", Some("root"), "Child"),
        ("grandchild", Some("child"), "Grandchild"),
    ] {
        let mut thread = ThreadRecord::new(ThreadId::new(id), project.clone(), title);
        thread.parent_thread_id = parent.map(ThreadId::new);
        thread.pinned = id == "child";
        sidebar.threads.push(thread);
    }
    sidebar.switch_thread(&ThreadId::new("grandchild")).unwrap();
    let state = WorkbenchState::new(EmptySource, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"));
    HeadlessWorkbench::new(state, [1600.0, 900.0])
}

#[test]
fn family_archive_and_restore_preserve_each_branch_expansion() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = fixture(temp.path());
    gui.run();
    assert_eq!(gui.count_labels("Archive"), 1);
    assert_eq!(gui.count_labels("Pause"), 3);
    assert!(gui.has_label("↳ Child"));
    assert!(gui.has_label("↳ Grandchild"));
    let parent_toggle = gui.label_rects("Collapse children of root")[0];
    let child_toggle = gui.label_rects("Collapse children of child")[0];
    assert!(child_toggle.left() > parent_toggle.left());
    assert!(child_toggle.top() > parent_toggle.top());

    // A collapsed root remains collapsed when the family moves to the archive.
    gui.click_label("Collapse children of root");
    gui.run();
    assert!(!gui.has_label("↳ Child"));
    assert!(!gui.has_label("↳ Grandchild"));
    gui.click_label("Archive");
    gui.run();
    assert!(
        gui.state()
            .sidebar()
            .threads
            .iter()
            .all(|thread| thread.archived)
    );
    assert!(gui.state().sidebar().active_thread.is_none());
    let saved = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert!(saved.threads.iter().all(|thread| thread.archived));
    assert!(saved.active_thread.is_none());

    gui.click_label("アーカイブ済み (3)");
    gui.run();
    assert!(gui.has_label("Expand children of root"));
    assert!(!gui.has_label("↳ Child"));
    assert_eq!(gui.count_labels("Restore"), 1);
    gui.click_label("Expand children of root");
    gui.run();
    assert!(gui.has_label("↳ Child"));
    assert!(gui.has_label("↳ Grandchild"));
    assert_eq!(gui.count_labels("Restore"), 1);
    let root_title = gui.label_rects("Parent")[0];
    let child_title = gui.label_rects("↳ Child")[0];
    assert!(child_title.left() > root_title.left());
    gui.click_label("↳ Child");
    gui.run();
    assert_eq!(
        gui.state().sidebar().active_thread,
        Some(ThreadId::new("child"))
    );
    gui.click_label("Collapse children of child");
    gui.run();
    assert!(!gui.has_label("↳ Grandchild"));

    // The child's independent expansion state survives restoring the parent.
    gui.click_label("Restore");
    gui.run();
    assert!(
        gui.state()
            .sidebar()
            .threads
            .iter()
            .all(|thread| !thread.archived)
    );
    assert!(gui.has_label("Collapse children of root"));
    assert!(gui.has_label("Expand children of child"));
    assert!(gui.has_label("↳ Child"));
    assert!(!gui.has_label("↳ Grandchild"));
    assert_eq!(gui.count_labels("Archive"), 1);
    assert_eq!(gui.count_labels("Pause"), 2);
    assert!(gui.has_label("★"));
    let saved = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert!(saved.threads.iter().all(|thread| !thread.archived));
}

#[test]
fn archiving_family_moves_active_descendant_to_unrelated_visible_thread() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = fixture(temp.path());
    let next = gui.state_mut().create_thread("Unrelated").unwrap();
    gui.state_mut()
        .switch_thread(ThreadId::new("grandchild"))
        .unwrap();
    gui.state_mut()
        .toggle_archive(ThreadId::new("root"))
        .unwrap();
    assert_eq!(gui.state().sidebar().active_thread, Some(next.clone()));
    let saved = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert_eq!(saved.active_thread, Some(next.clone()));
    assert!(
        !saved
            .threads
            .iter()
            .find(|thread| thread.id == next)
            .unwrap()
            .archived
    );
    assert_eq!(
        saved
            .threads
            .iter()
            .filter(|thread| thread.archived)
            .count(),
        3
    );
}

#[test]
fn direct_child_archive_and_restore_actions_do_not_split_the_family() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = fixture(temp.path());
    for archived in [false, true] {
        let before = gui.state().sidebar().clone();
        gui.state_mut()
            .toggle_archive(ThreadId::new("grandchild"))
            .unwrap();
        gui.state_mut()
            .toggle_archive(ThreadId::new("child"))
            .unwrap();
        assert_eq!(&before, gui.state().sidebar());
        assert!(
            gui.state()
                .sidebar()
                .threads
                .iter()
                .all(|thread| thread.archived == archived)
        );
        if !archived {
            gui.state_mut()
                .toggle_archive(ThreadId::new("root"))
                .unwrap();
        }
    }
}

#[test]
fn orphaned_and_cyclic_threads_are_reachable_in_both_sections() {
    let temp = tempfile::tempdir().unwrap();
    for archived in [false, true] {
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("project");
        sidebar
            .add_project(project.clone(), "Project", temp.path())
            .unwrap();
        sidebar.select_project(&project).unwrap();
        for (id, parent) in [
            ("Orphan", "missing"),
            ("Cycle A", "Cycle B"),
            ("Cycle B", "Cycle A"),
        ] {
            let mut thread = ThreadRecord::new(ThreadId::new(id), project.clone(), id);
            thread.parent_thread_id = Some(ThreadId::new(parent));
            thread.archived = archived;
            sidebar.threads.push(thread);
        }
        let state = WorkbenchState::new(EmptySource, &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar);
        let mut gui = HeadlessWorkbench::new(state, [1600.0, 900.0]);
        gui.run();
        if archived {
            gui.click_label("アーカイブ済み (3)");
            gui.run();
        }
        for label in ["↳ Orphan", "↳ Cycle A", "↳ Cycle B"] {
            assert_eq!(gui.count_labels(label), 1);
        }
        assert!(!gui.has_label("Archive"));
        assert!(!gui.has_label("Restore"));
    }
}
