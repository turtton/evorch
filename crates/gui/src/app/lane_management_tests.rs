//! Actual SQLite + locked sidebar host. Boundary ordering is explicit and synchronous.
#[path = "lane_management_tests/close.rs"]
mod close;
#[path = "lane_management_tests/runtime.rs"]
mod runtime;

use super::*;
use crate::{app::delivery_tests::state, fixture::DemoSource};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use storage::{Storage, StorageConfig, StorageHandle, lane_management::*, lanes::*};
use workspace_ui::{ThreadChatRole, ThreadId, UiSettings};

struct Host {
    handle: StorageHandle,
    obstruct: Option<PathBuf>,
}
impl LaneManagementHost for Host {
    fn apply(
        &self,
        request: LaneManagementRequest,
        guard: Option<LaneAttachGuard>,
    ) -> Result<LaneManagementProjection, String> {
        let id = request.lane_id.clone();
        self.handle
            .apply_lane_management_transition(request, LaneManagementActor::User, guard, 20)
            .map_err(|e| e.to_string())?;
        if let Some(path) = &self.obstruct {
            std::fs::rename(path, path.with_extension("before-crash")).unwrap();
            std::fs::create_dir(path).unwrap();
        }
        Ok(LaneManagementProjection {
            current_lane: self.handle.lane(&id).ok().flatten(),
        })
    }
}
fn window(path: &Path, handle: StorageHandle) -> WorkbenchState<DemoSource> {
    WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(workspace_ui::load_sidebar(path).unwrap())
        .with_sidebar_path(path.into())
        .with_thread_mirror(handle.clone())
        .with_lane_management_host(Arc::new(Host {
            handle,
            obstruct: None,
        }))
}
fn setup(dir: &Path) -> (WorkbenchState<DemoSource>, Storage, PathBuf) {
    let path = dir.join("sidebar.json");
    let storage = Storage::open(StorageConfig {
        db_path: dir.join("events.db"),
        ..Default::default()
    })
    .unwrap();
    let h = storage.handle();
    h.upsert_run_context(&storage::RunContextRecord {
        run_id: "run-1".into(),
        role: "Orchestrator".into(),
        name: "chat:Orchestrator:thread".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: "[]".into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: 1,
        host_inputs_json: "[]".into(),
    })
    .unwrap();
    h.upsert_threads_host(&[storage::threads::ThreadUpsert {
        thread_id: "thread".into(),
        project_id: Some("test".into()),
        project_root: Some(dir.to_string_lossy().into_owned()),
        root_run_id: Some("run-1".into()),
        chat_role: Some("Orchestrator".into()),
        ..Default::default()
    }])
    .unwrap();
    let lane = h
        .create_lane(NewLane {
            lane_id: "lane".into(),
            thread_id: "lane-thread".into(),
            origin_thread_id: "thread".into(),
            origin_run_id: "run-1".into(),
            creation_key: "open".into(),
            brief: LaneBrief {
                verification_requirements: Default::default(),
                objective: "implement".into(),
                inputs: vec![],
                deliverables: vec!["patch".into()],
                acceptance_criteria: vec!["tests".into()],
                constraints: vec![],
                workspace: Default::default(),
                report_policy: LanePolicy::Attached,
            },
            start_seed: "seed".into(),
            project_id: Some("test".into()),
            project_root: Some(dir.to_string_lossy().into_owned()),
            title: Some("lane".into()),
            at_ns: 1,
        })
        .unwrap()
        .lane;
    let mut ui = state(&path)
        .with_thread_mirror(h.clone())
        .with_lane_management_host(Arc::new(Host {
            handle: h,
            obstruct: None,
        }));
    ui.sidebar.threads[0].root_run_id = Some("run-1".into());
    ui.sidebar.threads[0].chat_role = Some(ThreadChatRole::Orchestrator);
    ui.sidebar
        .create_thread(
            ThreadId::new("lane-thread"),
            workspace_ui::ProjectId::new("test"),
            "lane",
        )
        .unwrap();
    ui.sidebar
        .apply_lane_link(
            &ThreadId::new("lane-thread"),
            crate::app::lanes::link_of(&lane),
        )
        .unwrap();
    ui.save_sidebar();
    ui.force_thread_mirror();
    (ui, storage, path)
}
fn detach(rev: u64) -> LaneManagementRequest {
    LaneManagementRequest {
        operation_id: format!("detach-{rev}"),
        lane_id: "lane".into(),
        expected_management_revision: rev,
        action: LaneManagementAction::Detach,
    }
}
fn repair(ui: &mut WorkbenchState<DemoSource>) {
    ui.shared_sidebar_transact(|_| Ok(())).unwrap();
    ui.force_thread_mirror();
}

#[test]
fn sql_commit_before_file_save_failure_repairs_on_restart_and_keeps_same_epoch_pin() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, path) = setup(dir.path());
    let h = storage.handle();
    ui.sidebar
        .set_family_pinned(&ThreadId::new("thread"), true)
        .unwrap();
    ui.save_sidebar();
    ui.force_thread_mirror();
    let mut stale = window(&path, h.clone());
    ui.lanes.management_host = Some(Arc::new(Host {
        handle: h.clone(),
        obstruct: Some(path.clone()),
    }));
    let error = ui.manage_lane(detach(1), None).unwrap_err();
    assert!(error.to_string().contains("committed"));
    assert_eq!(h.lane("lane").unwrap().unwrap().management_revision, 2);
    assert_eq!(
        h.pending_lane_management_projections(10).unwrap()[0].management_revision,
        2
    );
    // Restore only the pre-crash file: it still says managed + pinned.
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(path.with_extension("before-crash"), &path).unwrap();
    let mut restarted = window(&path, h.clone());
    repair(&mut restarted);
    let lane = ThreadId::new("lane-thread");
    assert_eq!(restarted.sidebar.family_key(&lane).unwrap(), lane);
    assert!(!restarted.sidebar.resolve_states()[&lane].pinned);
    assert!(!restarted.sidebar.resolve_states()[&lane].archived);
    assert!(
        h.pending_lane_management_projections(10)
            .unwrap()
            .is_empty()
    );
    restarted.toggle_pin(lane.clone()).unwrap();
    repair(&mut restarted);
    assert!(restarted.sidebar.resolve_states()[&lane].pinned);
    // Old windows cannot write the lead's state back into the independent family.
    stale
        .sidebar
        .set_family_archived(&ThreadId::new("thread"), true, 10)
        .unwrap();
    stale.save_sidebar();
    repair(&mut restarted);
    assert!(restarted.sidebar.resolve_states()[&lane].pinned);
    assert!(!restarted.sidebar.resolve_states()[&lane].archived);
    let db = storage::Database::open(&StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    })
    .unwrap();
    assert!(db.thread(&lane.to_string()).unwrap().unwrap().pinned);
}

#[test]
fn two_drops_and_a_stale_pin_have_one_membership_winner() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, storage, path) = setup(dir.path());
    a.manage_lane(detach(1), None).unwrap();
    let mut b = window(&path, storage.handle());
    let lane = ThreadId::new("lane-thread");
    let intent = workspace_ui::lane_management::LaneDropIntent {
        source: a.sidebar.lane_drag_intent(&lane).unwrap(),
        target_thread_id: ThreadId::new("thread"),
        expected_lead_root_run_id: Some("run-1".into()),
    };
    a.reattach_lane_drop(intent.clone()).unwrap();
    assert!(b.reattach_lane_drop(intent).is_err());
    assert!(b.toggle_pin(lane.clone()).is_err());
    let current = storage.handle().lane("lane").unwrap().unwrap();
    assert_eq!(current.management_revision, 3);
    assert_eq!(current.policy, LanePolicy::Summary);
    assert_eq!(current.lead_thread_id.as_deref(), Some("thread"));
    b.save_sidebar();
    repair(&mut a);
    assert_eq!(
        a.sidebar.family_key(&lane).unwrap(),
        ThreadId::new("thread")
    );
    assert!(!a.sidebar.resolve_states()[&lane].pinned);
}

#[test]
fn queued_stale_mirror_cannot_restore_old_flags_or_ack_a_new_revision() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, _) = setup(dir.path());
    let handle = storage.handle();
    let before = handle
        .sidebar_projection_snapshot(vec!["thread".into(), "lane-thread".into()])
        .unwrap();
    handle
        .apply_lane_management_transition(detach(1), LaneManagementActor::User, None, 20)
        .unwrap();
    let stale = storage::threads::ThreadUpsert {
        thread_id: "lane-thread".into(),
        pinned: Some(true),
        archived: Some(true),
        pin_revision: 999,
        archive_revision: 999,
        ..Default::default()
    };
    assert!(matches!(
        handle.project_sidebar_threads(vec![stale], before.fence, true),
        Err(storage::StorageError::LaneManagement(
            LaneManagementError::StaleManagementRevision
        ))
    ));
    assert_eq!(
        handle.pending_lane_management_projections(10).unwrap()[0].management_revision,
        2
    );
    // No management event is delivered. A mirror tick discovers the ledger
    // mismatch and saves its repair; the next tick mirrors and ACKs that file.
    ui.force_thread_mirror();
    ui.force_thread_mirror();
    assert!(
        handle
            .pending_lane_management_projections(10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn new_detach_discards_obsolete_high_revisions_before_raising_sql_floors() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, _) = setup(dir.path());
    let lane = ThreadId::new("lane-thread");
    ui.manage_lane(detach(1), None).unwrap();
    let intent = workspace_ui::lane_management::LaneDropIntent {
        source: ui.sidebar.lane_drag_intent(&lane).unwrap(),
        target_thread_id: ThreadId::new("thread"),
        expected_lead_root_run_id: Some("run-1".into()),
    };
    ui.reattach_lane_drop(intent).unwrap();
    ui.toggle_pin(ThreadId::new("thread")).unwrap();
    ui.force_thread_mirror();
    // An obsolete independent snapshot can survive in a stale window even
    // though the current attached family has different, lower SQL revisions.
    let old = ui
        .sidebar
        .families
        .iter_mut()
        .find(|state| state.key == lane)
        .unwrap();
    assert_eq!(old.membership_epoch, 2);
    old.pin_revision = 999;
    old.archive_revision = 999;
    old.pinned = true;
    old.archived = true;
    ui.save_sidebar();
    ui.manage_lane(detach(3), None).unwrap();
    repair(&mut ui);
    let state = &ui.sidebar.resolve_states()[&lane];
    assert!(!state.pinned && !state.archived);
    assert_eq!(
        ui.sidebar
            .lane_family_revision(&lane)
            .unwrap()
            .membership_epoch,
        4
    );
    let db = storage::Database::open(&StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    })
    .unwrap();
    let row = db.thread(&lane.to_string()).unwrap().unwrap();
    assert!(!row.pinned && !row.archived);
    assert!(
        storage
            .handle()
            .pending_lane_management_projections(10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn committed_but_unavailable_refresh_does_not_report_a_failed_management_change() {
    struct NoRefresh(StorageHandle);
    impl LaneManagementHost for NoRefresh {
        fn apply(
            &self,
            request: LaneManagementRequest,
            guard: Option<LaneAttachGuard>,
        ) -> Result<LaneManagementProjection, String> {
            self.0
                .apply_lane_management_transition(request, LaneManagementActor::User, guard, 20)
                .map_err(|e| e.to_string())?;
            Ok(LaneManagementProjection { current_lane: None })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, _) = setup(dir.path());
    ui.lanes.management_host = Some(Arc::new(NoRefresh(storage.handle())));
    let message = ui.manage_lane(detach(1), None).unwrap_err().to_string();
    assert!(message.contains("committed"));
    assert!(message.contains("repair"));
    assert_eq!(
        storage
            .handle()
            .lane("lane")
            .unwrap()
            .unwrap()
            .management_revision,
        2
    );
    repair(&mut ui);
    assert_eq!(
        ui.sidebar
            .family_key(&ThreadId::new("lane-thread"))
            .unwrap(),
        ThreadId::new("lane-thread")
    );
    assert!(
        storage
            .handle()
            .pending_lane_management_projections(10)
            .unwrap()
            .is_empty()
    );
}
