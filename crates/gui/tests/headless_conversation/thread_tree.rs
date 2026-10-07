use std::collections::{BTreeMap, BTreeSet};

use egui::epaint::{ColorMode, Shape};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::panes::sidebar::{SidebarAction, sidebar_pane};
use gui::theme::tokens::{SP_1, palette};
use runtime::{AgentInspection, AgentSummary, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRecord, ThreadRunPhase, UiSettings};

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }

    fn inspect(&self, _: RunId) -> Option<AgentInspection> {
        None
    }
}

fn fixture_with_size(root: &std::path::Path, size: [f32; 2]) -> HeadlessWorkbench<EmptySource> {
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
    HeadlessWorkbench::new(state, size)
}

fn fixture(root: &std::path::Path) -> HeadlessWorkbench<EmptySource> {
    fixture_with_size(root, [1600.0, 900.0])
}

const QUESTION_LABEL: &str = "Answer needed: this thread has an unanswered question";

fn status_fixture(
    phase: Option<ThreadRunPhase>,
    has_question: bool,
    width: f32,
) -> Harness<'static, Option<SidebarAction>> {
    let root = tempfile::tempdir().unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", root.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    let mut thread = ThreadRecord::new(ThreadId::new("status"), project, "Status thread");
    thread.run_ids.push("run-status".into());
    let questions = if has_question {
        BTreeSet::from([thread.id.clone()])
    } else {
        BTreeSet::new()
    };
    sidebar.threads.push(thread);
    let phases = phase
        .map(|phase| BTreeMap::from([("run-status".into(), phase)]))
        .unwrap_or_default();
    let telemetry = gui::model::telemetry::TelemetryOverlay::new();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(width, 400.0))
        .build_ui_state(
            move |ui, action| {
                let _keep_root = &root;
                gui::theme::install(ui.ctx());
                if let Some(next) = sidebar_pane(
                    ui,
                    &sidebar,
                    &phases,
                    &telemetry,
                    &questions,
                    &Default::default(),
                ) {
                    *action = Some(next);
                }
            },
            None,
        );
    // Spinners keep repainting; don't wait for the harness to settle.
    harness.run_steps(3);
    harness
}

#[test]
fn thread_status_icons_follow_state_and_question_priority() {
    for phase in [
        None,
        Some(ThreadRunPhase::Stopped),
        Some(ThreadRunPhase::Pending),
        Some(ThreadRunPhase::Running),
        Some(ThreadRunPhase::Waiting),
        Some(ThreadRunPhase::Done),
        Some(ThreadRunPhase::Error),
    ] {
        for has_question in [false, true] {
            let harness = status_fixture(phase, has_question, 600.0);
            // Pending is aggregated into ThreadState::Running by ThreadRecord.
            let running = matches!(
                phase,
                Some(ThreadRunPhase::Pending | ThreadRunPhase::Running)
            );
            let question = has_question;
            let error = phase == Some(ThreadRunPhase::Error);
            assert_eq!(
                harness.query_by_label("Thread status: Running").is_some(),
                running
            );
            assert_eq!(harness.query_by_label(QUESTION_LABEL).is_some(), question);
            assert_eq!(
                harness.query_by_label("Thread status: Error").is_some(),
                error
            );
            assert!(harness.query_by_label("?").is_none());
            for label in ["Active", "Stopped (resumable)", "Waiting", "Done"] {
                assert!(
                    harness
                        .query_by_label(&format!("Thread status: {label}"))
                        .is_none()
                );
            }
            assert_eq!(
                harness
                    .get_by_label("Archive")
                    .accesskit_node()
                    .is_disabled(),
                running
            );
            // Verify actual shapes rather than just accessible names.
            let shapes = &harness.output().shapes;
            let spinners = shapes
                .iter()
                .filter(|shape| {
                    matches!(&shape.shape, Shape::Path(path)
                    if !path.closed && path.stroke.color == ColorMode::Solid(palette().RUNNING))
                })
                .count();
            let blobs = shapes
                .iter()
                .filter(|shape| {
                    matches!(&shape.shape, Shape::Circle(circle)
                    if circle.fill == palette().WARNING_FG)
                })
                .count();
            // The error status is the Phosphor warning glyph painted in ERROR_FG.
            let warnings = shapes
                .iter()
                .filter(|shape| {
                    matches!(&shape.shape, Shape::Text(text)
                    if text.galley.text() == gui::theme::icons::WARNING
                        && text.galley.job.sections.iter().all(|section| section.format.color == palette().ERROR_FG))
                })
                .count();
            assert_eq!(spinners, usize::from(running));
            assert_eq!(blobs, usize::from(question));
            assert_eq!(warnings, usize::from(error));
        }
    }
}

#[test]
fn thread_status_icons_are_trailing_without_overlapping_title_or_actions() {
    for width in [280.0, 600.0] {
        for (phase, question, label) in [
            (ThreadRunPhase::Running, false, "Thread status: Running"),
            (ThreadRunPhase::Waiting, true, QUESTION_LABEL),
            (ThreadRunPhase::Error, false, "Thread status: Error"),
        ] {
            let mut harness = status_fixture(Some(phase), question, width);
            let title = harness.get_by_label("Status thread").rect();
            let icon = harness.get_by_label(label).rect();
            let action_label = if width < 300.0 { "⋯" } else { "Fork" };
            let action = harness.get_by_label(action_label).rect();
            assert!(title.right() <= action.left(), "{title:?} {action:?}");
            assert!(
                action.right() + SP_1 <= icon.left() + 0.5,
                "{action:?} {icon:?}"
            );
            assert!((title.center().y - icon.center().y).abs() <= 0.5);
            assert!(icon.right() <= width);
            assert!(title.right() <= harness.get_by_label("Archive").rect().left());
            // Moving the icon must not steal the title's or Fork's click target.
            harness.get_by_label("Status thread").click();
            harness.run_steps(3);
            assert_eq!(
                harness.state(),
                &Some(SidebarAction::SwitchThread(ThreadId::new("status")))
            );
            if action_label == "⋯" {
                harness.get_by_label(action_label).click();
                harness.run_steps(3);
            }
            harness.get_by_label("Fork").click();
            harness.run_steps(3);
            assert_eq!(
                harness.state(),
                &Some(SidebarAction::ForkThread(ThreadId::new("status")))
            );
        }
    }
}

#[test]
fn question_blob_and_runtime_icon_share_the_trailing_edge() {
    for width in [280.0, 600.0] {
        for (phase, status) in [
            (ThreadRunPhase::Pending, "Thread status: Running"),
            (ThreadRunPhase::Running, "Thread status: Running"),
            (ThreadRunPhase::Error, "Thread status: Error"),
        ] {
            let mut harness = status_fixture(Some(phase), true, width);
            let title = harness.get_by_label("Status thread").rect();
            let icon = harness.get_by_label(status).rect();
            let blob = harness.get_by_label(QUESTION_LABEL).rect();
            let action_label = if width < 300.0 { "⋯" } else { "Fork" };
            let action = harness.get_by_label(action_label).rect();
            assert!(title.right() <= action.left(), "{title:?} {action:?}");
            assert!(
                action.right() + SP_1 <= blob.left() + 0.5,
                "{action:?} {blob:?}"
            );
            assert!(
                (icon.left() - blob.right() - SP_1).abs() <= 0.5,
                "{blob:?} {icon:?}"
            );
            assert!((title.center().y - icon.center().y).abs() <= 0.5);
            assert!((blob.center().y - icon.center().y).abs() <= 0.5);
            let without_question = status_fixture(Some(phase), false, width);
            assert_eq!(
                icon.right(),
                without_question.get_by_label(status).rect().right()
            );
            assert!(icon.right() <= width);
            assert!(title.right() <= harness.get_by_label("Archive").rect().left());

            harness.get_by_label("Status thread").click();
            harness.run_steps(3);
            assert_eq!(
                harness.state(),
                &Some(SidebarAction::SwitchThread(ThreadId::new("status")))
            );
            if action_label == "⋯" {
                harness.get_by_label(action_label).click();
                harness.run_steps(3);
            }
            harness.get_by_label("Fork").click();
            harness.run_steps(3);
            assert_eq!(
                harness.state(),
                &Some(SidebarAction::ForkThread(ThreadId::new("status")))
            );
        }
    }
}

#[test]
fn family_rows_keep_title_and_actions_separate_at_minimum_width() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = fixture_with_size(temp.path(), gui::window::MIN_INNER_SIZE);
    gui.run();
    let root_title = gui.label_rects("Parent")[0];
    let root_pin = gui.label_rects("☆")[0];
    let archive = gui.label_rects("Archive")[0];
    let root_toggle = gui.label_rects("Collapse children of root")[0];
    // caret → title → pin → archive: the title leads, actions trail it.
    assert!(root_toggle.right() <= root_title.left());
    assert!(root_title.right() <= root_pin.left());
    assert!(root_pin.right() <= archive.left());
    let child_title = gui.label_rects("↳ Child")[0];
    let child_pin = gui.label_rects("★")[0];
    assert!(child_title.right() <= child_pin.left());
}

#[test]
fn family_archive_and_restore_preserve_each_branch_expansion() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = fixture(temp.path());
    gui.run();
    assert_eq!(gui.count_labels("Archive"), 1);
    assert_eq!(gui.count_labels("Fork"), 3);
    assert!(!gui.has_label("Pause"));
    assert!(!gui.has_label("Resume"));
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
    assert_eq!(gui.count_labels("Fork"), 2);
    assert!(!gui.has_label("Pause"));
    assert!(!gui.has_label("Resume"));
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
