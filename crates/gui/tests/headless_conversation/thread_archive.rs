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
    assert!(harness.has_label("☆"));
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

fn persisted_archive_run(
    storage: &storage::Storage,
    run: &str,
    thread: &str,
    phase: AgentRunPhase,
    snapshot: Option<&str>,
    restorable: bool,
) {
    for event in [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: run.into(),
            parent_run_id: None,
            agent_name: format!("chat:Worker:{thread}"),
            role: "Worker".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: run.into(),
            from: AgentRunPhase::Pending,
            to: phase,
            reason: None,
        }),
    ] {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    if let Some(snapshot) = snapshot {
        storage
            .handle()
            .upsert_run_context(&storage::RunContextRecord {
                run_id: run.into(),
                role: "Worker".into(),
                name: format!("chat:Worker:{thread}"),
                parent_run_id: None,
                config_json: "{}".into(),
                messages_json: "[]".into(),
                checkpoints_json: "[]".into(),
                terminal_phase: snapshot.into(),
                restorable,
                updated_at_ns: 1,
            })
            .unwrap();
    }
}

#[test]
fn restored_orphan_snapshots_reconcile_only_nonterminal_phases() {
    use workspace_ui::ThreadRunPhase as P;
    for (input, snapshot, expected, archive) in [
        (AgentRunPhase::Running, Some("Done"), P::Done, true),
        (AgentRunPhase::Running, Some("Checkpoint"), P::Stopped, true),
        (AgentRunPhase::Running, None, P::Running, false),
        (AgentRunPhase::Done, Some("Checkpoint"), P::Done, true),
        (AgentRunPhase::Pending, Some("Error"), P::Error, true),
        (AgentRunPhase::Waiting, Some("Stopped"), P::Stopped, true),
    ] {
        for restorable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let config = storage::StorageConfig {
                db_path: dir.path().join("events.db"),
                ..Default::default()
            };
            let storage = storage::Storage::open(config.clone()).unwrap();
            persisted_archive_run(
                &storage,
                "run-target",
                "target",
                input,
                snapshot,
                restorable,
            );
            storage.close();
            let registry = dir.path().join("owners.db");
            drop(runtime::ownership::Registry::open(&registry).unwrap());
            let mut state = fixture_state(dir.path(), false, false);
            state
                .restore_history_with_ownership(
                    &storage::Database::open(&config).unwrap(),
                    &registry,
                )
                .unwrap();
            assert_eq!(state.thread_phases()["run-target"], expected);
            state.toggle_archive(ThreadId::new("target")).unwrap();
            assert_eq!(state.sidebar().threads[0].archived, archive);
        }
    }
}

#[test]
fn restored_live_owner_and_legacy_turn_protect_archive() {
    for legacy in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        persisted_archive_run(
            &storage,
            "run-target",
            "target",
            AgentRunPhase::Running,
            Some("Checkpoint"),
            true,
        );
        storage.close();
        let host = runtime::ownership::OwnerHost::open(
            dir.path(),
            Default::default(),
            std::sync::Arc::new(event_bus::EventBus::new(32)),
        )
        .unwrap();
        let mut permit = host.start("different-owner-thread").unwrap();
        if !legacy {
            permit.run_id = Some("run-target".into());
        }
        permit.begin_turn().unwrap();
        let mut state = fixture_state(dir.path(), false, false);
        state
            .restore_history_with_ownership(
                &storage::Database::open(&config).unwrap(),
                &permit.registry_path,
            )
            .unwrap();
        assert_eq!(
            state.thread_phases()["run-target"],
            workspace_ui::ThreadRunPhase::Running
        );
        state.toggle_archive(ThreadId::new("target")).unwrap();
        assert!(!state.sidebar().threads[0].archived);
    }
}

#[test]
fn restored_owner_between_tool_rounds_blocks_archive_until_release_or_exit() {
    for release in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        persisted_archive_run(
            &storage,
            "run-target",
            "target",
            AgentRunPhase::Running,
            Some("Checkpoint"),
            true,
        );
        storage.close();
        let host = runtime::ownership::OwnerHost::open(
            dir.path(),
            Default::default(),
            std::sync::Arc::new(event_bus::EventBus::new(32)),
        )
        .unwrap();
        let mut permit = host.start("target").unwrap();
        permit.run_id = Some("run-target".into());
        permit.begin_turn().unwrap();
        // The agent loop checkpoints after tools while its phase stays Running.
        permit.checkpoint(&[]).unwrap();
        assert!(host.attach("target").unwrap().active_runs.is_empty());
        let db = storage::Database::open(&config).unwrap();
        let mut state = fixture_state(dir.path(), false, false);
        state
            .restore_history_with_ownership(&db, &permit.registry_path)
            .unwrap();
        assert_eq!(
            state.thread_phases()["run-target"],
            workspace_ui::ThreadRunPhase::Running
        );
        // Resuming the very next round must not expose an archiveable thread.
        permit.begin_turn().unwrap();
        state.toggle_archive(ThreadId::new("target")).unwrap();
        assert!(!state.sidebar().threads[0].archived);
        permit.checkpoint(&[]).unwrap();
        let host = if release {
            assert!(!host.quiesce().unwrap());
            // A responsive process owning another thread cannot keep the
            // released target's orphan from being reconciled.
            let mut unrelated = host.start("unrelated").unwrap();
            unrelated.run_id = Some("unrelated-run".into());
            unrelated.begin_turn().unwrap();
            Some(host)
        } else {
            drop(host);
            None
        };
        state
            .restore_history_with_ownership(&db, &permit.registry_path)
            .unwrap();
        assert_eq!(
            state.thread_phases()["run-target"],
            workspace_ui::ThreadRunPhase::Stopped
        );
        state.toggle_archive(ThreadId::new("target")).unwrap();
        assert!(state.sidebar().threads[0].archived);
        drop(host);
    }
}

#[test]
fn restored_checkpoint_probes_expired_owner_before_reconciling() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    persisted_archive_run(
        &storage,
        "run-target",
        "target",
        AgentRunPhase::Running,
        Some("Checkpoint"),
        true,
    );
    storage.close();
    let registry_path = dir.path().join("owners.db");
    let owner = runtime::ownership::ThreadOwner::new(
        "target".into(),
        runtime::ownership::Lease {
            owner_id: "a".repeat(32),
            generation: 1,
            expires_at: 0,
        },
    );
    runtime::ownership::Registry::open(&registry_path)
        .unwrap()
        .start(&owner)
        .unwrap();
    let socket = dir.path().join(format!("{}.sock", owner.lease.owner_id));
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let db = storage::Database::open(&config).unwrap();
    let mut state = fixture_state(dir.path(), false, false);
    state
        .restore_history_with_ownership(&db, &registry_path)
        .unwrap();
    assert_eq!(
        state.thread_phases()["run-target"],
        workspace_ui::ThreadRunPhase::Running
    );
    // Model an owner exit without a graceful release, leaving a stale socket.
    drop(listener);
    for stale_socket in [true, false] {
        if !stale_socket {
            std::fs::remove_file(&socket).unwrap();
        }
        state
            .restore_history_with_ownership(&db, &registry_path)
            .unwrap();
        assert_eq!(
            state.thread_phases()["run-target"],
            workspace_ui::ThreadRunPhase::Stopped
        );
    }
    state.toggle_archive(ThreadId::new("target")).unwrap();
    assert!(state.sidebar().threads[0].archived);
}

#[test]
fn restored_checkpoint_preserves_phase_when_owner_liveness_is_unknown() {
    for invalid_id in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        persisted_archive_run(
            &storage,
            "run-target",
            "target",
            AgentRunPhase::Running,
            Some("Checkpoint"),
            true,
        );
        storage.close();
        let registry_path = dir.path().join("owners.db");
        let owner = runtime::ownership::ThreadOwner::new(
            "target".into(),
            runtime::ownership::Lease {
                owner_id: if invalid_id {
                    "../invalid-owner".into()
                } else {
                    "a".repeat(32)
                },
                generation: 1,
                expires_at: 0,
            },
        );
        runtime::ownership::Registry::open(&registry_path)
            .unwrap()
            .start(&owner)
            .unwrap();
        if !invalid_id {
            // ELOOP is an unknown endpoint failure, not proof of owner death.
            let socket = dir.path().join(format!("{}.sock", owner.lease.owner_id));
            std::os::unix::fs::symlink(&socket, &socket).unwrap();
        }
        let mut state = fixture_state(dir.path(), false, false);
        state
            .restore_history_with_ownership(
                &storage::Database::open(&config).unwrap(),
                &registry_path,
            )
            .unwrap();
        assert_eq!(
            state.thread_phases()["run-target"],
            workspace_ui::ThreadRunPhase::Running
        );
        state.toggle_archive(ThreadId::new("target")).unwrap();
        assert!(!state.sidebar().threads[0].archived);
    }
}

#[test]
fn restored_orphan_with_live_descendant_cannot_archive_family() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = fixture_state(dir.path(), false, false);
    let child = state.fork_thread(ThreadId::new("target")).unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    persisted_archive_run(
        &storage,
        "orphan",
        "target",
        AgentRunPhase::Running,
        Some("Done"),
        false,
    );
    persisted_archive_run(
        &storage,
        "live",
        &child.to_string(),
        AgentRunPhase::Running,
        Some("Checkpoint"),
        true,
    );
    storage.close();
    let host = runtime::ownership::OwnerHost::open(
        dir.path(),
        Default::default(),
        std::sync::Arc::new(event_bus::EventBus::new(32)),
    )
    .unwrap();
    let mut permit = host.start("another-owner").unwrap();
    permit.run_id = Some("live".into());
    permit.begin_turn().unwrap();
    state
        .restore_history_with_ownership(
            &storage::Database::open(&config).unwrap(),
            &permit.registry_path,
        )
        .unwrap();
    assert_eq!(
        state.thread_phases()["orphan"],
        workspace_ui::ThreadRunPhase::Done
    );
    assert_eq!(
        state.thread_phases()["live"],
        workspace_ui::ThreadRunPhase::Running
    );
    state.toggle_archive(ThreadId::new("target")).unwrap();
    assert!(
        state
            .sidebar()
            .threads
            .iter()
            .all(|thread| !thread.archived)
    );
}

#[test]
fn unreadable_ownership_never_reconciles_restored_runs() {
    for corrupt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        persisted_archive_run(
            &storage,
            "run-target",
            "target",
            AgentRunPhase::Running,
            Some("Done"),
            false,
        );
        storage.close();
        let registry = dir.path().join("owners.db");
        if corrupt {
            std::fs::write(&registry, "invalid database").unwrap();
        }
        let mut state = fixture_state(dir.path(), false, false);
        state
            .restore_history_with_ownership(&storage::Database::open(&config).unwrap(), &registry)
            .unwrap();
        assert_eq!(
            state.thread_phases()["run-target"],
            workspace_ui::ThreadRunPhase::Running
        );
        state.toggle_archive(ThreadId::new("target")).unwrap();
        assert!(!state.sidebar().threads[0].archived);
    }
}
