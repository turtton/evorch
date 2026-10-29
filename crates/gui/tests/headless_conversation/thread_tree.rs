use std::collections::{BTreeMap, BTreeSet};

use egui::epaint::{ColorMode, Shape};
use egui_kittest::{Harness, kittest::Queryable};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::panes::sidebar::{SidebarAction, sidebar_pane};
use gui::theme::tokens::{SP_1, palette};
use runtime::{AgentInspection, AgentSummary, RunId};
use workspace_ui::{
    LineageKind, ProjectId, SidebarState, ThreadId, ThreadLineage, ThreadRecord, ThreadRunPhase,
    UiSettings,
};

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

/// Running families hide Archive, leaving Pin as the trailing row action.
fn trailing_action(harness: &Harness<'_, Option<SidebarAction>>) -> egui::Rect {
    harness
        .query_by_label("Archive")
        .unwrap_or_else(|| harness.get_by_label("☆"))
        .rect()
}

fn status_fixture(
    phase: Option<ThreadRunPhase>,
    has_question: bool,
    width: f32,
) -> Harness<'static, Option<SidebarAction>> {
    attention_fixture(phase, (has_question, false), width)
}

fn attention_fixture(
    phase: Option<ThreadRunPhase>,
    (has_question, has_approval): (bool, bool),
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
    let flagged = |flag: bool| {
        if flag {
            BTreeSet::from([thread.id.clone()])
        } else {
            BTreeSet::new()
        }
    };
    let questions = flagged(has_question);
    let approvals = flagged(has_approval);
    sidebar.threads.push(thread);
    let phases = phase
        .map(|phase| BTreeMap::from([("run-status".into(), phase)]))
        .unwrap_or_default();
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
                    &questions,
                    &approvals,
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
                harness.query_by_label("Thread status: Failed").is_some(),
                error
            );
            assert!(harness.query_by_label("?").is_none());
            for label in ["Active", "Stopped", "Waiting", "Done"] {
                assert!(
                    harness
                        .query_by_label(&format!("Thread status: {label}"))
                        .is_none()
                );
            }
            assert_eq!(harness.query_by_label("Archive").is_none(), running);
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
        // The running spinner sits in the Archive slot instead; see
        // running_spinner_takes_the_archive_slot_beside_attention_dots.
        for (phase, question, label) in [
            (ThreadRunPhase::Waiting, true, QUESTION_LABEL),
            (ThreadRunPhase::Error, false, "Thread status: Failed"),
        ] {
            let mut harness = status_fixture(Some(phase), question, width);
            let title = harness.get_by_label("Status thread").rect();
            let icon = harness.get_by_label(label).rect();
            let action = trailing_action(&harness);
            assert!(title.right() <= action.left(), "{title:?} {action:?}");
            assert!(
                action.right() + SP_1 <= icon.left() + 0.5,
                "{action:?} {icon:?}"
            );
            assert!((title.center().y - icon.center().y).abs() <= 0.5);
            assert!(icon.right() <= width);
            // Moving the icon must not steal the title's or Pin's click target.
            harness.get_by_label("Status thread").click();
            harness.run_steps(3);
            assert_eq!(
                harness.state(),
                &Some(SidebarAction::SwitchThread(ThreadId::new("status")))
            );
            harness.get_by_label("☆").click();
            harness.run_steps(3);
            assert_eq!(
                harness.state(),
                &Some(SidebarAction::TogglePin(ThreadId::new("status")))
            );
        }
    }
}

#[test]
fn question_blob_and_error_icon_share_the_trailing_edge() {
    for width in [280.0, 600.0] {
        let (phase, status) = (ThreadRunPhase::Error, "Thread status: Failed");
        let mut harness = status_fixture(Some(phase), true, width);
        let title = harness.get_by_label("Status thread").rect();
        let icon = harness.get_by_label(status).rect();
        let blob = harness.get_by_label(QUESTION_LABEL).rect();
        let action = trailing_action(&harness);
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

        harness.get_by_label("Status thread").click();
        harness.run_steps(3);
        assert_eq!(
            harness.state(),
            &Some(SidebarAction::SwitchThread(ThreadId::new("status")))
        );
        harness.get_by_label("☆").click();
        harness.run_steps(3);
        assert_eq!(
            harness.state(),
            &Some(SidebarAction::TogglePin(ThreadId::new("status")))
        );
    }
}

#[test]
fn running_spinner_takes_the_archive_slot_beside_attention_dots() {
    for width in [280.0, 600.0] {
        for has_question in [false, true] {
            // Given: the same row idle, then with a pending or running run.
            let idle = status_fixture(None, has_question, width);
            let idle_archive = idle.get_by_label("Archive").rect();
            let idle_pin = idle.get_by_label("☆").rect();
            for phase in [ThreadRunPhase::Pending, ThreadRunPhase::Running] {
                let mut harness = status_fixture(Some(phase), has_question, width);
                // Then: the spinner replaces Archive in place and Pin stays put.
                let spinner = harness.get_by_label("Thread status: Running").rect();
                assert!(harness.query_by_label("Archive").is_none());
                assert_eq!(harness.get_by_label("☆").rect(), idle_pin);
                assert!(
                    (spinner.center() - idle_archive.center()).length() <= 0.5,
                    "{idle_archive:?} {spinner:?}"
                );
                if has_question {
                    let blob = harness.get_by_label(QUESTION_LABEL).rect();
                    assert!(spinner.right() <= blob.left(), "{spinner:?} {blob:?}");
                    assert!(blob.right() <= width);
                }
                // The spinner must not steal Pin's click target.
                harness.get_by_label("☆").click();
                harness.run_steps(3);
                assert_eq!(
                    harness.state(),
                    &Some(SidebarAction::TogglePin(ThreadId::new("status")))
                );
            }
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
    // Only a displayed root can be pinned: children, even with a stale pin, show no Pin.
    assert!(gui.has_label("↳ Child"));
    assert!(!gui.has_label("★"));
    assert_eq!(gui.count_labels("☆"), 1);
}

#[test]
fn toggling_pin_on_a_child_thread_is_a_noop() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = fixture(temp.path());
    gui.run();
    let before = gui.state().sidebar().clone();
    // Stale or direct actions on a child must not change it; a root still toggles.
    gui.state_mut().toggle_pin(ThreadId::new("child")).unwrap();
    gui.state_mut()
        .toggle_pin(ThreadId::new("grandchild"))
        .unwrap();
    assert_eq!(gui.state().sidebar(), &before);
    gui.state_mut().toggle_pin(ThreadId::new("root")).unwrap();
    let root = gui
        .state()
        .sidebar()
        .threads
        .iter()
        .find(|thread| thread.id == ThreadId::new("root"))
        .unwrap();
    assert!(root.pinned);
}

#[test]
fn family_archive_and_restore_preserve_each_branch_expansion() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = fixture(temp.path());
    gui.run();
    assert_eq!(gui.count_labels("Archive"), 1);
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

    gui.click_label("Archived (3)");
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
    assert!(!gui.has_label("Pause"));
    assert!(!gui.has_label("Resume"));
    assert!(!gui.has_label("★"));
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
            gui.click_label("Archived (3)");
            gui.run();
        }
        for label in ["↳ Orphan", "↳ Cycle A", "↳ Cycle B"] {
            assert_eq!(gui.count_labels(label), 1);
        }
        assert!(!gui.has_label("Archive"));
        assert!(!gui.has_label("Restore"));
    }
}

const APPROVAL_LABEL: &str = "Approval needed: a tool call is waiting";

#[test]
fn pending_approval_marks_the_thread_distinctly_from_questions() {
    let warning_glyphs = |harness: &Harness<'_, Option<SidebarAction>>| {
        harness
            .output()
            .shapes
            .iter()
            .filter(|shape| {
                matches!(&shape.shape, Shape::Text(text)
                    if text.galley.text() == gui::theme::icons::SHIELD_WARNING)
            })
            .count()
    };
    let dots = |harness: &Harness<'_, Option<SidebarAction>>| {
        harness
            .output()
            .shapes
            .iter()
            .filter(|shape| {
                matches!(&shape.shape, Shape::Circle(circle)
                    if circle.fill == palette().WARNING_FG)
            })
            .count()
    };
    // Given: a waiting thread with only a pending approval.
    let harness = attention_fixture(Some(ThreadRunPhase::Waiting), (false, true), 600.0);
    // Then: it gets the amber attention dot with approval-specific text.
    assert!(harness.query_by_label(APPROVAL_LABEL).is_some());
    assert!(harness.query_by_label(QUESTION_LABEL).is_none());
    assert_eq!((dots(&harness), warning_glyphs(&harness)), (1, 0));

    // Given: the same thread with both a question and an approval.
    let harness = attention_fixture(Some(ThreadRunPhase::Waiting), (true, true), 600.0);
    // Then: the question keeps the dot and the approval uses its own glyph.
    assert!(harness.query_by_label(APPROVAL_LABEL).is_some());
    assert!(harness.query_by_label(QUESTION_LABEL).is_some());
    assert_eq!((dots(&harness), warning_glyphs(&harness)), (1, 1));
}

#[test]
fn thread_context_menu_copies_the_full_thread_id_and_title() {
    for (item, expected) in [
        ("Copy thread ID", "status"),
        ("Copy thread title", "Status thread"),
    ] {
        // Given: a thread row.
        let mut harness = status_fixture(None, false, 600.0);
        // When: the row is right-clicked and a copy item is chosen.
        harness.get_by_label("Status thread").click_secondary();
        harness.run_steps(2);
        harness.get_by_label(item).click();
        harness.run_steps(1);
        // Then: the clipboard receives the value, the menu closes and the
        // right-click did not switch threads.
        assert!(
            harness.output().platform_output.commands.iter().any(
                |command| matches!(command, egui::OutputCommand::CopyText(text) if text == expected)
            ),
            "{item}"
        );
        harness.run_steps(2);
        assert!(harness.query_by_label(item).is_none(), "{item} menu closed");
        assert_eq!(harness.state(), &None);
    }
}

/// Solo B, Solo A, Parent { Escalated { Nested }, Fork }, Solo C, newest first.
/// Thread IDs per indicator for [`branch_fixture_with`].
#[derive(Default)]
struct BranchFlags {
    running: &'static [&'static str],
    questions: &'static [&'static str],
    approvals: &'static [&'static str],
    unread: &'static [&'static str],
}

fn branch_fixture_with_width(width: f32) -> Harness<'static, Option<SidebarAction>> {
    branch_fixture_with(width, BranchFlags::default())
}

fn branch_fixture_with(width: f32, flags: BranchFlags) -> Harness<'static, Option<SidebarAction>> {
    let root = tempfile::tempdir().unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", root.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for (id, parent, title, updated_at) in [
        ("solo-b", None, "Solo B", 300),
        ("solo-a", None, "Solo A", 200),
        ("root", None, "Parent", 100),
        ("esc", Some("root"), "Escalated", 90),
        ("nested", Some("esc"), "Nested", 85),
        ("fork", Some("root"), "Fork", 80),
        ("solo-c", None, "Solo C", 50),
    ] {
        let mut thread = ThreadRecord::new(ThreadId::new(id), project.clone(), title);
        thread.parent_thread_id = parent.map(ThreadId::new);
        thread.created_at = updated_at;
        thread.updated_at = updated_at;
        if matches!(id, "esc" | "nested") {
            thread.escalation_source_run_id = Some(format!("run-worker-{id}"));
        }
        if id == "fork" {
            thread.lineage = Some(ThreadLineage {
                kind: LineageKind::Fork,
                point: None,
            });
        }
        thread.run_ids.push(format!("run-{id}"));
        sidebar.threads.push(thread);
    }
    let ids =
        |ids: &[&str]| -> BTreeSet<ThreadId> { ids.iter().copied().map(ThreadId::new).collect() };
    let phases: BTreeMap<String, ThreadRunPhase> = flags
        .running
        .iter()
        .map(|id| (format!("run-{id}"), ThreadRunPhase::Running))
        .collect();
    let (questions, approvals, unread) = (
        ids(flags.questions),
        ids(flags.approvals),
        ids(flags.unread),
    );
    let mut harness = Harness::builder()
        .with_size(egui::vec2(width, 360.0))
        .build_ui_state(
            move |ui, action| {
                let _keep_root = &root;
                gui::theme::install(ui.ctx());
                if let Some(next) =
                    sidebar_pane(ui, &sidebar, &phases, &questions, &approvals, &unread)
                {
                    *action = Some(next);
                }
            },
            None,
        );
    harness.run_steps(3);
    harness
}

#[test]
fn child_rows_name_why_they_are_nested_beside_their_titles() {
    let harness = branch_fixture();
    let rect = |label: &str| harness.get_by_label(label).rect();
    let kind_rects = |label: &str| {
        harness
            .query_all_by_label(label)
            .map(|node| node.rect())
            .collect::<Vec<_>>()
    };
    // Roots carry no kind marker; each child names its origin.
    let escalated = kind_rects("Escalated thread");
    let forked = kind_rects("Forked thread");
    assert_eq!((escalated.len(), forked.len()), (2, 1));
    let in_row = |marker: &egui::Rect, title: egui::Rect| {
        (marker.center().y - title.center().y).abs() <= 1.0 && marker.right() <= title.left()
    };
    // A parent child keeps its caret, then the marker, then the title.
    let toggle = rect("Collapse children of esc");
    let title = rect("↳ Escalated");
    let marker = escalated.iter().find(|r| in_row(r, title)).unwrap();
    assert!(toggle.right() <= marker.left(), "{toggle:?} {marker:?}");
    // A leaf child shows its marker in the empty caret slot.
    let nested = rect("↳ Nested");
    let marker = escalated.iter().find(|r| in_row(r, nested)).unwrap();
    assert!(marker.left() > toggle.left(), "{marker:?} {toggle:?}");
    let fork = rect("↳ Fork");
    assert!(in_row(&forked[0], fork));
    assert!((forked[0].left() - toggle.left()).abs() <= 0.5);
}

#[test]
fn families_with_children_are_spaced_apart_from_neighbouring_rows() {
    let harness = branch_fixture();
    let top = |label: &str| harness.get_by_label(label).rect().top();
    let pitch = top("Solo A") - top("Solo B");
    assert!(pitch > 0.0);
    // Inside a family, rows keep the ordinary pitch.
    assert!((top("↳ Escalated") - top("Parent") - pitch).abs() <= 0.5);
    // Entering and leaving the family adds the family gap.
    assert!((top("Parent") - top("Solo A") - (pitch + SP_1)).abs() <= 0.5);
    assert!((top("Solo C") - top("↳ Fork") - (pitch + SP_1)).abs() <= 0.5);
}

#[test]
fn tree_rails_join_children_to_their_parent_and_stop_at_the_last_child() {
    let harness = branch_fixture();
    let rect = |label: &str| harness.get_by_label(label).rect();
    // Vertical segments drawn at `x`, merged top to bottom.
    let rail = |x: f32| {
        let mut spans: Vec<(f32, f32)> = harness
            .output()
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                Shape::LineSegment { points: [a, b], .. }
                    if (a.x - b.x).abs() < 0.01 && (a.x - x).abs() <= 0.5 =>
                {
                    Some((a.y.min(b.y), a.y.max(b.y)))
                }
                _ => None,
            })
            .collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert!(!spans.is_empty(), "no rail at x={x}");
        for pair in spans.windows(2) {
            assert!(pair[1].0 - pair[0].1 <= 1.0, "rail gap {spans:?}");
        }
        (
            spans[0].0,
            spans.iter().map(|span| span.1).fold(f32::MIN, f32::max),
        )
    };
    // The root's rail runs from its caret to the middle of its last child.
    let root_toggle = rect("Collapse children of root");
    let (top, bottom) = rail(root_toggle.center().x);
    assert!(top > root_toggle.center().y && top < root_toggle.bottom());
    assert!((bottom - rect("↳ Fork").center().y).abs() <= 1.0);
    // The nested rail ends at Nested even though the root's continues past it.
    let esc_toggle = rect("Collapse children of esc");
    let (top, bottom) = rail(esc_toggle.center().x);
    assert!(top > esc_toggle.center().y && top < esc_toggle.bottom());
    assert!((bottom - rect("↳ Nested").center().y).abs() <= 1.0);
}

fn branch_fixture() -> Harness<'static, Option<SidebarAction>> {
    branch_fixture_with_width(600.0)
}

/// Work that needs the user, all inside Parent's subtree.
fn flagged_children() -> BranchFlags {
    BranchFlags {
        running: &["nested"],
        questions: &["nested"],
        approvals: &["fork"],
        unread: &["fork"],
    }
}

const CHILD_QUESTION: &str = "Answer needed: a collapsed child thread has an unanswered question";
const CHILD_APPROVAL: &str = "Approval needed: a tool call is waiting in a collapsed child thread";
const CHILD_RUNNING: &str = "Running in a collapsed child thread";
const CHILD_UNREAD: &str = "Unread report in a collapsed child thread";

fn collapse(harness: &mut Harness<'static, Option<SidebarAction>>, id: &str) {
    harness
        .get_by_label(&format!("Collapse children of {id}"))
        .click();
    // Spinners keep repainting; don't wait for the harness to settle.
    harness.run_steps(3);
}

fn same_row(harness: &Harness<'_, Option<SidebarAction>>, label: &str, title: &str) -> bool {
    let title = harness.get_by_label(title).rect();
    harness
        .query_all_by_label(label)
        .any(|node| (node.rect().center().y - title.center().y).abs() <= 1.0)
}

#[test]
fn collapsed_parent_rolls_up_what_its_hidden_children_need() {
    let mut harness = branch_fixture_with(600.0, flagged_children());
    // Given: expanded, each child carries its own indicators.
    assert_eq!(harness.query_all_by_label(QUESTION_LABEL).count(), 1);
    assert_eq!(
        harness.query_all_by_label("Thread status: Running").count(),
        1
    );
    for label in [CHILD_QUESTION, CHILD_APPROVAL, CHILD_RUNNING, CHILD_UNREAD] {
        assert!(harness.query_by_label(label).is_none(), "{label}");
    }
    assert!(harness.query_by_label("3 hidden child threads").is_none());

    // When: Parent is collapsed.
    collapse(&mut harness, "root");

    // Then: Parent's row carries a count and everything its children need.
    assert!(harness.query_by_label("↳ Nested").is_none());
    assert!(harness.query_by_label(QUESTION_LABEL).is_none());
    assert!(harness.query_by_label("Thread status: Running").is_none());
    for label in [
        CHILD_QUESTION,
        CHILD_APPROVAL,
        CHILD_RUNNING,
        CHILD_UNREAD,
        "3 hidden child threads",
    ] {
        assert_eq!(harness.query_all_by_label(label).count(), 1, "{label}");
        assert!(same_row(&harness, label, "Parent"), "{label}");
    }
    // A question and an approval stay distinguishable: dot plus shield glyph.
    let shields = harness
        .output()
        .shapes
        .iter()
        .filter(|shape| {
            matches!(&shape.shape, Shape::Text(text)
                if text.galley.text() == gui::theme::icons::SHIELD_WARNING)
        })
        .count();
    assert_eq!(shields, 1);
    // The spinner takes Parent's Archive slot; childless roots keep Archive.
    let spinner = harness.get_by_label(CHILD_RUNNING).rect();
    let archive = harness.query_all_by_label("Archive").next().unwrap().rect();
    assert_eq!(harness.query_all_by_label("Archive").count(), 3);
    assert_eq!(spinner.width(), archive.width());
    // The count sits between the title and Pin.
    let badge = harness.get_by_label("3 hidden child threads").rect();
    let title = harness.get_by_label("Parent").rect();
    let pin = harness
        .query_all_by_label("☆")
        .map(|node| node.rect())
        .find(|pin| (pin.center().y - title.center().y).abs() <= 1.0)
        .unwrap();
    assert!(title.right() <= badge.left(), "{title:?} {badge:?}");
    assert!(badge.right() <= pin.left(), "{badge:?} {pin:?}");

    // When: Parent is expanded again, the children speak for themselves.
    harness.get_by_label("Expand children of root").click();
    harness.run_steps(3);
    assert_eq!(harness.query_all_by_label(QUESTION_LABEL).count(), 1);
    for label in [CHILD_QUESTION, CHILD_APPROVAL, CHILD_RUNNING, CHILD_UNREAD] {
        assert!(harness.query_by_label(label).is_none(), "{label}");
    }
    assert!(harness.query_by_label("3 hidden child threads").is_none());
}

#[test]
fn a_collapsed_child_rolls_up_only_its_own_subtree() {
    let mut harness = branch_fixture_with(600.0, flagged_children());
    collapse(&mut harness, "esc");
    assert_eq!(
        harness.query_all_by_label("1 hidden child thread").count(),
        1
    );
    for label in ["1 hidden child thread", CHILD_QUESTION, CHILD_RUNNING] {
        assert!(same_row(&harness, label, "↳ Escalated"), "{label}");
    }
    // Fork is still visible, so its approval and unread report stay its own.
    for label in [CHILD_APPROVAL, CHILD_UNREAD] {
        assert!(harness.query_by_label(label).is_none(), "{label}");
    }
    assert!(same_row(&harness, APPROVAL_LABEL, "↳ Fork"));
    assert!(!same_row(&harness, CHILD_QUESTION, "Parent"));
}

#[test]
fn a_row_s_own_attention_takes_precedence_over_its_hidden_children() {
    let mut harness = branch_fixture_with(
        600.0,
        BranchFlags {
            questions: &["root", "nested"],
            ..BranchFlags::default()
        },
    );
    collapse(&mut harness, "root");
    assert_eq!(harness.query_all_by_label(QUESTION_LABEL).count(), 1);
    assert!(same_row(&harness, QUESTION_LABEL, "Parent"));
    assert!(harness.query_by_label(CHILD_QUESTION).is_none());
}

#[test]
#[ignore = "writes thread tree PNG evidence using an offscreen adapter"]
fn capture_thread_tree_png_evidence() {
    let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gui-evidence/thread-tree");
    std::fs::create_dir_all(&output).expect("evidence directory");
    let mut harness = branch_fixture_with(320.0, flagged_children());
    harness.run_steps(2);
    let image = harness.render().expect("offscreen adapter");
    image
        .save(output.join("thread-tree.png"))
        .expect("thread tree PNG");
    collapse(&mut harness, "root");
    let image = harness.render().expect("offscreen adapter");
    image
        .save(output.join("thread-tree-collapsed.png"))
        .expect("collapsed thread tree PNG");
}
