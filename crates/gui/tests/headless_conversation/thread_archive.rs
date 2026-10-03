use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use event_bus::{AgentRunPhase, Event, LifecycleEvent};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use runtime::{AgentInspection, AgentSummary, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
    fn inspect(&self, _: RunId) -> Option<AgentInspection> {
        None
    }
}

fn fixture(root: &std::path::Path, archived: bool) -> HeadlessWorkbench<EmptySource> {
    fixture_with_pinned(root, archived, false)
}

fn fixture_with_pinned(
    root: &std::path::Path,
    archived: bool,
    pinned: bool,
) -> HeadlessWorkbench<EmptySource> {
    HeadlessWorkbench::new(fixture_state(root, archived, pinned), [1000.0, 700.0])
}

fn fixture_state(
    root: &std::path::Path,
    archived: bool,
    pinned: bool,
) -> WorkbenchState<EmptySource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("p");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("target"), project, "Target")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("target")).unwrap();
    // Use the persisted shape so the pre-implementation UI test can run.
    let mut json = serde_json::to_value(sidebar).unwrap();
    json["threads"][0]["archived"] = archived.into();
    json["threads"][0]["pinned"] = pinned.into();
    json["threads"][0]["created_at"] = 0.into();
    WorkbenchState::new(EmptySource, &UiSettings::default())
        .unwrap()
        .with_sidebar(serde_json::from_value(json).unwrap())
        .with_sidebar_path(root.join("sidebar.json"))
}

fn set_thread_phase(
    state: &mut WorkbenchState<EmptySource>,
    thread: &ThreadId,
    phase: AgentRunPhase,
) {
    let run_id = format!("run-{thread}");
    state.apply_events([
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: run_id.clone(),
            parent_run_id: None,
            agent_name: format!("chat:Worker:{thread}"),
            role: "worker".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id,
            from: AgentRunPhase::Pending,
            to: phase,
            reason: None,
        }),
    ]);
}

#[test]
fn running_thread_archive_button_is_disabled_and_direct_action_is_noop() {
    for phase in [AgentRunPhase::Pending, AgentRunPhase::Running] {
        // Given: a thread whose aggregated state is Running.
        let temp = tempfile::tempdir().unwrap();
        let mut harness = fixture(temp.path(), false);
        let target = ThreadId::new("target");
        set_thread_phase(harness.state_mut(), &target, phase);
        harness.run();
        let before = harness.state().sidebar().clone();
        let saved = std::fs::read(temp.path().join("sidebar.json")).unwrap();
        assert!(harness.has_label("Thread status: Running"));

        // When: clicking the disabled control, then bypassing the UI.
        harness.click_label("Archive");
        harness.run();
        assert_eq!(harness.state().sidebar(), &before);
        harness.state_mut().toggle_archive(target).unwrap();

        // Then: neither path changes selection, archive state or persistence.
        assert_eq!(harness.state().sidebar(), &before);
        assert_eq!(
            std::fs::read(temp.path().join("sidebar.json")).unwrap(),
            saved
        );
    }
}

fn assert_archive_blocked(state: WorkbenchState<EmptySource>, root: &std::path::Path) {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1000.0, 700.0))
        .build_ui_state(
            |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
            state,
        );
    harness.ctx.global_style_mut(|style| {
        style.interaction.tooltip_delay = 0.0;
        style.interaction.show_tooltips_only_when_still = false;
    });
    harness.run_steps(3);
    let before = harness.state().sidebar().clone();
    let saved = std::fs::read(root.join("sidebar.json")).unwrap();
    let archive = harness.get_by_label("Archive");
    assert!(archive.accesskit_node().is_disabled());
    archive.hover();
    harness.run_steps(3);
    assert!(
        harness
            .query_by_label("Archive unavailable while a thread in this family is running")
            .is_some()
    );

    harness.get_by_label("Archive").click();
    harness.run_steps(3);
    assert_eq!(harness.state().sidebar(), &before);
    assert_eq!(std::fs::read(root.join("sidebar.json")).unwrap(), saved);

    harness
        .state_mut()
        .toggle_archive(ThreadId::new("target"))
        .unwrap();
    assert_eq!(harness.state().sidebar(), &before);
    assert_eq!(std::fs::read(root.join("sidebar.json")).unwrap(), saved);
}

#[test]
fn mixed_phases_block_archive_despite_stopped_or_error_display_state() {
    for terminal in [AgentRunPhase::Stopped, AgentRunPhase::Error] {
        for active in [AgentRunPhase::Running, AgentRunPhase::Pending] {
            // Exercise both a mixed root and a mixed descendant hidden by a
            // non-running root. Child runs inherit the same conversation owner.
            for descendant in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let mut state = fixture_state(temp.path(), false, false);
                let target = ThreadId::new("target");
                let owner = if descendant {
                    let child = state.fork_thread(target.clone()).unwrap();
                    state.fork_thread(child).unwrap()
                } else {
                    target
                };
                set_thread_phase(&mut state, &owner, terminal);
                state.apply_events([
                    Event::new(LifecycleEvent::AgentRunStarted {
                        run_id: "active-child-run".into(),
                        parent_run_id: Some(format!("run-{owner}")),
                        agent_name: "Child worker".into(),
                        role: "worker".into(),
                    }),
                    Event::new(LifecycleEvent::AgentRunStateChanged {
                        run_id: "active-child-run".into(),
                        from: AgentRunPhase::Pending,
                        to: active,
                        reason: None,
                    }),
                ]);
                let thread = state
                    .sidebar()
                    .threads
                    .iter()
                    .find(|thread| thread.id == owner)
                    .unwrap();
                assert_eq!(
                    thread.run_ids,
                    [format!("run-{owner}"), "active-child-run".into()]
                );
                assert_archive_blocked(state, temp.path());
            }
        }
    }
}

#[test]
fn running_descendant_prevents_archiving_the_entire_family() {
    for phase in [AgentRunPhase::Running, AgentRunPhase::Pending] {
        // Given: an idle root with a running/pending grandchild.
        let temp = tempfile::tempdir().unwrap();
        let mut state = fixture_state(temp.path(), false, false);
        let child = state.fork_thread(ThreadId::new("target")).unwrap();
        let grandchild = state.fork_thread(child).unwrap();
        set_thread_phase(&mut state, &grandchild, phase);

        // Both the root control and direct action must preserve the whole family.
        assert_archive_blocked(state, temp.path());
    }
}

#[test]
fn stopped_or_completed_thread_can_be_archived() {
    for phase in [
        AgentRunPhase::Stopped,
        AgentRunPhase::Done,
        AgentRunPhase::Error,
        AgentRunPhase::Waiting,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut harness = fixture(temp.path(), false);
        let target = ThreadId::new("target");
        set_thread_phase(harness.state_mut(), &target, phase);
        harness.run();
        harness.click_label("Archive");
        harness.run();
        assert!(harness.state().sidebar().threads[0].archived, "{phase:?}");
    }
}

#[test]
fn running_archived_thread_can_still_be_restored() {
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), true);
    let target = ThreadId::new("target");
    set_thread_phase(harness.state_mut(), &target, AgentRunPhase::Running);
    harness.state_mut().toggle_archive(target).unwrap();
    assert!(!harness.state().sidebar().threads[0].archived);
    let saved = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert!(!saved.threads[0].archived);
}

#[test]
fn archive_control_keeps_a_comfortable_hitbox() {
    // Given: a visible unpinned thread.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), false);
    // When: laying out its icon-only archive control.
    harness.run();
    // Then: the accessible control has at least a 20-point square hitbox.
    let rect = harness.label_rects("Archive")[0];
    assert!(rect.width() >= 20.0 && rect.height() >= 20.0, "{rect:?}");
}

#[test]
fn pinned_thread_archive_button_disabled_noop() {
    // Given: one visible pinned thread.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture_with_pinned(temp.path(), false, true);
    harness.run();

    // When: attempting to click its archive control.
    harness.click_label("Archive");
    harness.run();

    // Then: the pinned thread remains visible and unarchived.
    assert!(harness.has_label("Target"));
    assert!(!harness.state().sidebar().threads[0].archived);
}

#[test]
fn unpinned_thread_archive_still_works() {
    // Given: one visible unpinned thread.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture_with_pinned(temp.path(), false, false);
    harness.run();

    // When: clicking its archive control.
    harness.click_label("Archive");
    harness.run();

    // Then: the thread is archived through the real dispatch path.
    assert!(!harness.has_label("Target"));
    assert!(harness.state().sidebar().threads[0].archived);
}

#[test]
fn archive_click_moves_thread_out_of_main_and_persists() {
    // Given: one visible, active thread and a real JSON persistence path.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), false);
    harness.run();
    // When: clicking the row's archive control through the real viewer dispatch.
    harness.click_label("Archive");
    harness.run();
    // Then: the closed archive hides the title, clears selection and persists.
    assert!(harness.has_label("アーカイブ済み (1)"));
    assert!(!harness.has_label("Target"));
    assert!(!harness.has_label("Pause"));
    assert!(harness.state().sidebar().active_thread.is_none());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("sidebar.json")).unwrap()).unwrap();
    assert_eq!(saved["threads"][0]["archived"], true);
    harness.click_label("アーカイブ済み (1)");
    harness.run();
    assert_eq!(harness.count_labels("Target"), 1);
    assert!(harness.has_label("Restore"));
}

#[test]
fn restore_click_returns_archived_thread_to_main_and_persists() {
    // Given: a persisted archive displayed in the expanded archive section.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), true);
    harness.run();
    harness.click_label("アーカイブ済み (1)");
    harness.run();
    // When: restoring the archived row.
    harness.click_label("Restore");
    harness.run();
    // Then: the main row and its original actions return, with no duplicate title.
    assert_eq!(harness.count_labels("Target"), 1);
    assert!(harness.has_label("Archive"));
    // Idle rows no longer reserve a status dot, leaving room for the inline action.
    assert!(harness.has_label("Fork"));
    assert!(!harness.has_label("Restore"));
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("sidebar.json")).unwrap()).unwrap();
    assert_eq!(saved["threads"][0]["archived"], false);
}

#[test]
fn fork_inherits_parent_archive_state_and_has_a_new_creation_date() {
    // Given: an archived source with a legacy creation date.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), true);
    // When: forking it through the workbench API.
    let id = harness
        .state_mut()
        .fork_thread(ThreadId::new("target"))
        .unwrap();
    // Then: the new child inherits its family's archive state and is persisted.
    let fork = harness
        .state()
        .sidebar()
        .threads
        .iter()
        .find(|t| t.id == id)
        .unwrap();
    assert!(fork.archived);
    assert!(fork.created_at > 0);
    let saved = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert!(
        saved
            .threads
            .iter()
            .find(|thread| thread.id == id)
            .unwrap()
            .archived
    );
    harness
        .state_mut()
        .toggle_archive(ThreadId::new("target"))
        .unwrap();
    assert!(
        harness
            .state()
            .sidebar()
            .threads
            .iter()
            .all(|thread| !thread.archived)
    );
}

#[test]
fn archive_selects_next_visible_thread_when_active_has_a_successor() {
    // Given: the target is active and another visible thread exists.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), false);
    let next = harness.state_mut().create_thread("Next").unwrap();
    harness
        .state_mut()
        .switch_thread(ThreadId::new("target"))
        .unwrap();
    // When: archiving the active thread.
    harness
        .state_mut()
        .toggle_archive(ThreadId::new("target"))
        .unwrap();
    // Then: selection moves to the visible successor and is persisted.
    assert_eq!(
        harness.state().sidebar().active_thread.as_ref(),
        Some(&next)
    );
    let saved = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert_eq!(saved.active_thread, Some(next));
}

#[test]
fn archived_title_switches_thread_when_clicked() {
    // Given: an archived row and a different active conversation.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), true);
    harness.state_mut().create_thread("Next").unwrap();
    harness.run();
    harness.click_label("アーカイブ済み (1)");
    harness.run();
    // When: selecting the archived title without restoring it.
    harness.click_label("Target");
    harness.run();
    // Then: the archived conversation becomes active but remains archived.
    assert_eq!(
        harness.state().sidebar().active_thread,
        Some(ThreadId::new("target"))
    );
    assert!(harness.state().sidebar().threads[0].archived);
}

#[test]
fn unknown_archive_target_returns_error_without_changing_sidebar() {
    // Given: a valid sidebar and an unknown thread identifier.
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), false);
    let before = harness.state().sidebar().clone();
    // When: attempting to archive the unknown thread.
    let result = harness.state_mut().toggle_archive(ThreadId::new("missing"));
    // Then: a typed error preserves the sidebar.
    assert!(matches!(
        result,
        Err(gui::app::WorkbenchError::Thread(
            workspace_ui::ThreadError::UnknownThread
        ))
    ));
    assert_eq!(harness.state().sidebar(), &before);
}

#[test]
#[ignore = "writes native offscreen archive evidence"]
fn capture_archive_states() {
    let temp = tempfile::tempdir().unwrap();
    let mut harness = fixture(temp.path(), false);
    harness.run();
    harness
        .capture()
        .unwrap()
        .save_png(std::path::Path::new("/tmp/opencode/archive-main.png"))
        .unwrap();
    harness.click_label("Archive");
    harness.run();
    harness
        .capture()
        .unwrap()
        .save_png(std::path::Path::new("/tmp/opencode/archive-closed.png"))
        .unwrap();
    harness.click_label("アーカイブ済み (1)");
    harness.run();
    harness
        .capture()
        .unwrap()
        .save_png(std::path::Path::new("/tmp/opencode/archive-open.png"))
        .unwrap();
}

#[test]
#[ignore = "writes native offscreen icon evidence"]
fn capture_archive_icons() {
    for (pinned, name) in [(false, "enabled"), (true, "disabled")] {
        let temp = tempfile::tempdir().unwrap();
        let mut capture = HeadlessWorkbench::with_pixels_per_point(
            fixture_state(temp.path(), false, pinned),
            [1000.0, 700.0],
            3.0,
        );
        capture.run();
        capture
            .capture()
            .unwrap()
            .save_png(std::path::Path::new(&format!(
                "/tmp/opencode/archive-icon-{name}.png"
            )))
            .unwrap();
    }
}
