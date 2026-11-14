//! The lane ledger (ADR 0044 Phase 1b): identity, management, reports, `lane_list`.

use std::collections::BTreeSet;

use event_bus::LedgerEvent as Ev;
use rusqlite::Connection;
use storage::StorageError;
use storage::lanes::{
    LANE_LIST_PAGE_MAX, LANE_LIST_PAGE_MAX_BYTES, LANE_LIST_SNAPSHOTS_PER_LEAD, LaneBrief,
    LaneConflictReason, LaneCursorError, LaneListPage, LaneListRequest, LanePolicy,
    LaneReportInput, LaneReportProducer, LaneScope, LaneStatus, LaneValidationKind,
    LaneWorkspaceDeclaration, NewLane,
};
use storage::threads::{ThreadLineageKind, ThreadUpsert};
use storage::{Database, Storage, StorageConfig, StorageHandle};

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("lanes.db"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
        }
    }

    fn handle(&self) -> StorageHandle {
        self.storage.handle()
    }

    fn database(&self) -> Database {
        Database::open(&self.config).unwrap()
    }

    fn sql(&self) -> Connection {
        let conn = Connection::open(&self.config.db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }
}

fn brief(objective: &str, policy: LanePolicy) -> LaneBrief {
    LaneBrief {
        verification_requirements: Default::default(),
        objective: objective.into(),
        inputs: vec![],
        deliverables: vec!["a pull request".into()],
        acceptance_criteria: vec!["tests pass".into()],
        constraints: vec![],
        workspace: LaneWorkspaceDeclaration {
            branch: Some(format!("feat/{objective}")),
            worktree: None,
            notes: None,
        },
        report_policy: policy,
    }
}

fn new_lane(n: u32, at_ns: i64) -> NewLane {
    NewLane {
        lane_id: format!("lane-{n:03}"),
        thread_id: format!("thread-{n:03}"),
        origin_thread_id: "lead".into(),
        origin_run_id: "lead-run".into(),
        creation_key: format!("call-{n:03}"),
        brief: brief(&format!("objective-{n:03}"), LanePolicy::Attached),
        start_seed: "seed".into(),
        project_id: Some("project".into()),
        project_root: Some("/repo".into()),
        title: Some(format!("Lane {n}")),
        at_ns,
    }
}

fn report(status: LaneStatus, summary: &str) -> LaneReportInput {
    LaneReportInput {
        artifact_revision: None,
        failure_episode: None,
        status,
        summary: summary.into(),
        artifact_refs: vec![],
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: LaneReportProducer::LaneAgent,
    }
}

fn list_request(cursor: Option<String>) -> LaneListRequest {
    LaneListRequest {
        lead_thread_id: "lead".into(),
        policy: None,
        cursor,
        limit: LANE_LIST_PAGE_MAX,
    }
}

fn ids(page: &LaneListPage) -> Vec<String> {
    page.items.iter().map(|item| item.lane_id.clone()).collect()
}

fn page_bytes(page: &LaneListPage) -> usize {
    serde_json::to_vec(page).unwrap().len()
}

fn all_pages(handle: &StorageHandle) -> Vec<String> {
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = handle.lane_list(list_request(cursor)).unwrap();
        seen.extend(ids(&page));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return seen,
        }
    }
}

#[test]
fn a_retried_creation_returns_the_same_lane_and_never_a_second_one() {
    // Given: a lane created under a creation key.
    let f = Fixture::new();
    let handle = f.handle();
    let first = handle.create_lane(new_lane(1, 10)).unwrap();
    assert!(first.created);

    // When: the same request is repeated, even with fresh IDs a retry might generate.
    let mut retry = new_lane(1, 99);
    retry.lane_id = "lane-retry".into();
    retry.thread_id = "thread-retry".into();
    let second = handle.create_lane(retry).unwrap();

    // Then: the first lane comes back unchanged, no event is due and nothing was added.
    assert!(!second.created);
    assert_eq!(second.lane, first.lane);
    assert!(handle.lane("lane-retry").unwrap().is_none());
    assert_eq!(all_pages(&handle), vec!["lane-001"]);
}

#[test]
fn a_creation_key_or_id_that_belongs_to_another_request_is_a_conflict() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();

    // The key was used for a different brief.
    let mut other_brief = new_lane(1, 11);
    other_brief.brief.objective = "something else".into();
    assert_eq!(
        handle.create_lane(other_brief).unwrap_err(),
        StorageError::LaneConflict(LaneConflictReason::CreationKeyReused)
    );

    // A new key cannot take the lane or thread ID of another lane.
    let mut same_lane_id = new_lane(2, 12);
    same_lane_id.lane_id = "lane-001".into();
    assert_eq!(
        handle.create_lane(same_lane_id).unwrap_err(),
        StorageError::LaneConflict(LaneConflictReason::IdentityTaken)
    );
    let mut same_thread = new_lane(3, 13);
    same_thread.thread_id = "thread-001".into();
    assert_eq!(
        handle.create_lane(same_thread).unwrap_err(),
        StorageError::LaneConflict(LaneConflictReason::IdentityTaken)
    );

    // A thread that already exists as a fork is not turned into a lane.
    handle
        .upsert_thread_runtime(&ThreadUpsert {
            thread_id: "thread-fork".into(),
            parent_thread_id: Some("lead".into()),
            lineage_kind: Some(ThreadLineageKind::Fork),
            created_at_ns: 1,
            updated_at_ns: 1,
            ..ThreadUpsert::default()
        })
        .unwrap();
    let mut on_fork = new_lane(4, 14);
    on_fork.thread_id = "thread-fork".into();
    assert_eq!(
        handle.create_lane(on_fork).unwrap_err(),
        StorageError::LaneConflict(LaneConflictReason::ThreadNotLane)
    );
    assert_eq!(all_pages(&handle), vec!["lane-001"]);
}

#[test]
fn creation_stores_the_origin_the_scope_and_the_lane_thread_row() {
    let f = Fixture::new();
    let created = f.handle().create_lane(new_lane(1, 10)).unwrap().lane;

    assert_eq!(created.origin_thread_id, "lead");
    assert_eq!(created.origin_run_id, "lead-run");
    assert_eq!(created.lead_thread_id.as_deref(), Some("lead"));
    assert_eq!(created.policy, LanePolicy::Attached);
    assert_eq!(created.scope, LaneScope::Lane);
    assert!(!created.scope.can_open_lanes());
    assert_eq!(
        (created.management_revision, created.brief_revision),
        (1, 1)
    );
    assert!(!created.is_started());
    assert_eq!(created.start_seed, "seed");
    let thread = f.database().thread("thread-001").unwrap().unwrap();
    assert_eq!(thread.lineage_kind, Some(ThreadLineageKind::Lane));
    assert_eq!(thread.project_root.as_deref(), Some("/repo"));
    // The management relation is the ledger's; the thread row names no parent.
    assert_eq!(thread.parent_thread_id, None);
}

#[test]
fn a_lane_created_detached_has_no_lead_but_keeps_its_origin() {
    let f = Fixture::new();
    let mut detached = new_lane(1, 10);
    detached.brief.report_policy = LanePolicy::Detached;
    let lane = f.handle().create_lane(detached).unwrap().lane;

    assert_eq!(lane.policy, LanePolicy::Detached);
    assert_eq!(lane.lead_thread_id, None);
    assert_eq!(lane.origin_thread_id, "lead");
    assert_eq!(lane.origin_run_id, "lead-run");
    // A detached lane is in no lead's listing.
    assert_eq!(all_pages(&f.handle()), Vec::<String>::new());
}

#[test]
fn input_that_is_empty_oversized_or_secret_is_refused_and_stores_nothing() {
    let f = Fixture::new();
    let handle = f.handle();

    let mut no_criteria = new_lane(1, 10);
    no_criteria.brief.acceptance_criteria.clear();
    let error = handle.create_lane(no_criteria).unwrap_err();
    assert!(matches!(
        error,
        StorageError::LaneInvalid(ref e) if e.field == "acceptance_criteria"
    ));

    let mut huge_seed = new_lane(2, 10);
    huge_seed.start_seed = "s".repeat(64 * 1024 + 1);
    assert!(matches!(
        handle.create_lane(huge_seed).unwrap_err(),
        StorageError::LaneInvalid(ref e) if e.field == "start_seed"
    ));

    let mut secret = new_lane(3, 10);
    secret.brief.objective = format!("use sk-{}", "abcdefghijklmnopqrstuvwxyz0123456789");
    assert!(matches!(
        handle.create_lane(secret).unwrap_err(),
        StorageError::SecretDetected { .. }
    ));

    // Nothing was stored, thread rows included.
    assert_eq!(all_pages(&handle), Vec::<String>::new());
    assert!(f.database().thread("thread-001").unwrap().is_none());
    assert!(f.database().thread("thread-003").unwrap().is_none());
}

#[test]
fn management_changes_move_from_the_revision_the_caller_read() {
    use storage::lane_management::*;
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();
    let request = LaneManagementRequest {
        operation_id: "summary".into(),
        lane_id: "lane-001".into(),
        expected_management_revision: 1,
        action: LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    };
    let summary = handle
        .apply_lane_management_transition(request.clone(), LaneManagementActor::User, None, 20)
        .unwrap();
    assert!(summary.changed);
    assert_eq!(summary.lane.management_revision, 2);
    assert_eq!(summary.lane.lead_thread_id.as_deref(), Some("lead"));
    let mut stale = request.clone();
    stale.operation_id = "stale".into();
    assert_eq!(
        handle
            .apply_lane_management_transition(stale, LaneManagementActor::User, None, 21)
            .unwrap_err(),
        StorageError::LaneManagement(LaneManagementError::StaleManagementRevision)
    );
    let mut same = request;
    same.operation_id = "same".into();
    same.expected_management_revision = 2;
    assert!(
        !handle
            .apply_lane_management_transition(same, LaneManagementActor::User, None, 22)
            .unwrap()
            .changed
    );
}

#[test]
fn detaching_keeps_the_origin_and_a_host_mirror_cannot_undo_or_hide_it() {
    // Given: an attached lane.
    let f = Fixture::new();
    let handle = f.handle();
    let created = handle.create_lane(new_lane(1, 10)).unwrap().lane;

    // When: the data step of a detach is taken.
    let detached = handle
        .apply_lane_management_transition(
            storage::lane_management::LaneManagementRequest {
                operation_id: format!("transition:{}", 20),
                lane_id: ("lane-001").to_string(),
                expected_management_revision: 1,
                action: storage::lane_management::LaneManagementAction::Detach,
            },
            storage::lane_management::LaneManagementActor::User,
            None,
            20,
        )
        .unwrap()
        .lane;

    // Then: the management relation ended; the immutable origin did not move.
    assert_eq!(detached.lead_thread_id, None);
    assert_eq!(detached.management_revision, 2);
    assert_eq!(detached.origin_thread_id, created.origin_thread_id);
    assert_eq!(detached.origin_run_id, created.origin_run_id);
    assert_eq!(detached.creation_key, created.creation_key);
    assert_eq!(detached.created_at_ns, created.created_at_ns);
    assert_eq!(all_pages(&handle), Vec::<String>::new());

    // And: a stale host mirror that sends a parent and a fork label (or none) leaves
    // the lane thread a lane, and never turns the parent link into the management.
    handle
        .upsert_threads_host(&[ThreadUpsert {
            thread_id: "thread-001".into(),
            parent_thread_id: Some("lead".into()),
            lineage_kind: Some(ThreadLineageKind::Fork),
            created_at_ns: 1,
            updated_at_ns: 30,
            ..ThreadUpsert::default()
        }])
        .unwrap();
    handle
        .upsert_threads_host(&[ThreadUpsert {
            thread_id: "thread-001".into(),
            parent_thread_id: None,
            lineage_kind: None,
            created_at_ns: 1,
            updated_at_ns: 31,
            ..ThreadUpsert::default()
        }])
        .unwrap();
    let thread = f.database().thread("thread-001").unwrap().unwrap();
    assert_eq!(thread.lineage_kind, Some(ThreadLineageKind::Lane));
    let lane = handle.lane("thread-001").unwrap();
    assert!(lane.is_none(), "lane IDs and thread IDs are different keys");
    let by_thread = handle.lane_by_thread("thread-001").unwrap().unwrap();
    assert_eq!(by_thread.policy, LanePolicy::Detached);
    assert_eq!(by_thread.lead_thread_id, None);
}

#[test]
fn the_database_itself_refuses_to_rewrite_origin_scope_counters_or_reports() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();
    handle
        .append_lane_report("lane-001", 1, report(LaneStatus::Working, "one"), 11)
        .unwrap();
    let conn = f.sql();

    for statement in [
        "UPDATE lanes SET origin_thread_id = 'x' WHERE lane_id = 'lane-001'",
        "UPDATE lanes SET origin_run_id = 'x' WHERE lane_id = 'lane-001'",
        "UPDATE lanes SET creation_key = 'x' WHERE lane_id = 'lane-001'",
        "UPDATE lanes SET management_revision = 0 WHERE lane_id = 'lane-001'",
        "UPDATE lanes SET latest_report_seq = 0 WHERE lane_id = 'lane-001'",
        "UPDATE lanes SET lead_thread_id = NULL WHERE lane_id = 'lane-001'",
        "UPDATE lane_reports SET summary = 'rewritten' WHERE lane_id = 'lane-001'",
        "UPDATE threads SET lineage_kind = 'fork' WHERE thread_id = 'thread-001'",
        // A gap in the sequence.
        "INSERT INTO lane_reports(lane_id, report_seq, status, summary, artifact_refs_json, \
         needs_human, question_refs_json, producer, management_revision, brief_revision, \
         created_at_ns) VALUES ('lane-001', 5, 'working', 's', '[]', 0, '[]', 'harness', 1, 1, 1)",
        // Reuse of a number.
        "INSERT INTO lane_reports(lane_id, report_seq, status, summary, artifact_refs_json, \
         needs_human, question_refs_json, producer, management_revision, brief_revision, \
         created_at_ns) VALUES ('lane-001', 1, 'working', 's', '[]', 0, '[]', 'harness', 1, 1, 1)",
    ] {
        assert!(
            conn.execute(statement, []).is_err(),
            "must be refused: {statement}"
        );
    }
}

#[test]
fn reports_are_numbered_by_the_ledger_in_order_and_never_change() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();

    let mut statuses = vec![];
    for (index, status) in [LaneStatus::Working, LaneStatus::Blocked, LaneStatus::Ready]
        .into_iter()
        .enumerate()
    {
        let appended = handle
            .append_lane_report(
                "lane-001",
                1,
                report(status, &format!("step {index}")),
                100 + index as i64,
            )
            .unwrap();
        assert_eq!(appended.report.report_seq, index as u64 + 1);
        assert_eq!(appended.lane.latest_report_seq, index as u64 + 1);
        assert_eq!(appended.report.management_revision, 1);
        statuses.push(appended.report.status);
    }

    let stored = f.database().lane_reports("lane-001", 0, 50).unwrap();
    assert_eq!(
        stored.iter().map(|r| r.report_seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        stored.iter().map(|r| r.status).collect::<Vec<_>>(),
        statuses
    );
    assert_eq!(
        f.database()
            .lane_reports("lane-001", 2, 50)
            .unwrap()
            .iter()
            .map(|r| r.summary.clone())
            .collect::<Vec<_>>(),
        ["step 2"]
    );

    // The latest report is what `lane_list` shows; it is a claim, never a verified close.
    let page = handle.lane_list(list_request(None)).unwrap();
    let latest = page.items[0].latest_report.as_ref().unwrap();
    assert_eq!((latest.report_seq, latest.status), (3, LaneStatus::Ready));
}

#[test]
fn oversized_empty_or_secret_reports_are_errors_and_leave_no_trace() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();
    let before = handle.lane("lane-001").unwrap().unwrap();

    let mut long = report(LaneStatus::Working, &"x".repeat(1025));
    let error = handle
        .append_lane_report("lane-001", 1, long.clone(), 11)
        .unwrap_err();
    assert!(matches!(
        error,
        StorageError::LaneInvalid(ref e) if e.field == "summary"
            && matches!(e.kind, LaneValidationKind::TooLarge { max: 1024, actual: 1025 })
    ));
    long.summary = "ok".into();
    long.artifact_refs = vec!["a".into(); 9];
    assert!(matches!(
        handle.append_lane_report("lane-001", 1, long, 12).unwrap_err(),
        StorageError::LaneInvalid(ref e)
            if matches!(e.kind, LaneValidationKind::TooMany { max: 8, actual: 9 })
    ));
    assert!(matches!(
        handle
            .append_lane_report("lane-001", 1, report(LaneStatus::Ready, " "), 13)
            .unwrap_err(),
        StorageError::LaneInvalid(ref e) if e.kind == LaneValidationKind::Empty
    ));
    assert!(matches!(
        handle
            .append_lane_report(
                "lane-001",
                1,
                report(
                    LaneStatus::Ready,
                    &format!("token sk-{}", "abcdefghijklmnopqrstuvwxyz0123456789")
                ),
                14
            )
            .unwrap_err(),
        StorageError::SecretDetected { .. }
    ));
    assert!(matches!(
        handle
            .append_lane_report("missing", 1, report(LaneStatus::Ready, "x"), 15)
            .unwrap_err(),
        StorageError::LaneNotFound { .. }
    ));

    // Nothing was written: the sequence still starts at 1 and the lane is as before.
    assert_eq!(handle.lane("lane-001").unwrap().unwrap(), before);
    assert!(
        f.database()
            .lane_reports("lane-001", 0, 50)
            .unwrap()
            .is_empty()
    );
    let first = handle
        .append_lane_report("lane-001", 1, report(LaneStatus::Working, "fine"), 16)
        .unwrap();
    assert_eq!(first.report.report_seq, 1);
}

#[test]
fn a_root_start_is_a_compare_and_swap_and_unstarted_lanes_are_found_again() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();
    handle.create_lane(new_lane(2, 11)).unwrap();

    // Both creation intents are durable before any root runs.
    let pending = f.database().unstarted_lanes(10).unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|l| l.lane_id.as_str())
            .collect::<Vec<_>>(),
        ["lane-001", "lane-002"]
    );

    let started = handle.start_lane_run("lane-001", 0, "root-1", 20).unwrap();
    assert_eq!(
        (started.generation, started.root_run_id.as_deref()),
        (1, Some("root-1"))
    );
    assert!(started.is_started());
    assert_eq!(
        handle
            .start_lane_run("lane-001", 0, "root-other", 21)
            .unwrap_err(),
        StorageError::StaleRevision {
            kind: "generation",
            expected: 0,
            current: 1
        }
    );
    let restarted = handle.start_lane_run("lane-001", 1, "root-2", 22).unwrap();
    assert_eq!(
        (restarted.generation, restarted.root_run_id.as_deref()),
        (2, Some("root-2"))
    );
    assert_eq!(
        f.database()
            .unstarted_lanes(10)
            .unwrap()
            .iter()
            .map(|l| l.lane_id.as_str())
            .collect::<Vec<_>>(),
        ["lane-002"]
    );
}

#[test]
fn v45_migration_keeps_pending_v44_intents_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("lanes.db"),
        ..Default::default()
    };
    let original = {
        let storage = Storage::open(config.clone()).unwrap();
        let lane = storage.handle().create_lane(new_lane(1, 10)).unwrap().lane;
        storage.close();
        lane
    };
    {
        let conn = Connection::open(&config.db_path).unwrap();
        conn.execute_batch(concat!(
            concat!(
                include_str!("../../undo/undo_v61.sql"),
                include_str!("../../undo/undo_v60.sql"),
                include_str!("../../undo/undo_v59.sql"),
                include_str!("../../undo/undo_v58.sql"),
                include_str!("../../undo/undo_v57.sql"),
                include_str!("../../undo/undo_v56.sql"),
                include_str!("../../undo/undo_v55.sql"),
                include_str!("../../undo/undo_v54.sql"),
                include_str!("../../undo/undo_v53.sql"),
                include_str!("../../undo/undo_v52.sql"),
                include_str!("../../undo/undo_v51.sql"),
                include_str!("../../undo/undo_v50.sql"),
                include_str!("../../undo/undo_v49.sql"),
                include_str!("../../undo/undo_v48.sql"),
                include_str!("../../undo/undo_v47.sql")
            ),
            include_str!("../../undo/undo_v46.sql")
        ))
        .unwrap();
        conn.execute_batch(
            "ALTER TABLE lanes DROP COLUMN start_blocked_reason; PRAGMA user_version=44;",
        )
        .unwrap();
    }
    let storage = Storage::open(config).unwrap();
    let h = storage.handle();
    assert_eq!(h.unstarted_lanes(10).unwrap(), [original]);
    assert!(
        h.block_lane_start("lane-001", "cancelled", 20)
            .unwrap()
            .changed
    );
    assert!(h.unstarted_lanes(10).unwrap().is_empty());
    storage.close();
}

#[test]
fn a_blocked_intent_is_durable_idempotent_and_cannot_start_but_started_lanes_are_unchanged() {
    let f = Fixture::new();
    let h = f.handle();
    h.create_lane(new_lane(1, 10)).unwrap();
    h.create_lane(new_lane(2, 11)).unwrap();
    let stopped = h.block_lane_start("lane-001", "cancelled", 12).unwrap();
    assert!(stopped.changed);
    let again = h
        .block_lane_start("lane-001", "another reason", 13)
        .unwrap();
    assert!(!again.changed);
    assert_eq!(again.lane, stopped.lane);
    let reopened = f.database();
    assert_eq!(reopened.lane("lane-001").unwrap().unwrap(), stopped.lane);
    assert_eq!(
        reopened
            .unstarted_lanes(10)
            .unwrap()
            .iter()
            .map(|l| l.lane_id.as_str())
            .collect::<Vec<_>>(),
        ["lane-002"]
    );
    assert_eq!(
        h.start_lane_run("lane-001", 0, "root-1", 14).unwrap_err(),
        StorageError::LaneConflict(LaneConflictReason::StartBlocked)
    );
    let retry = h.create_lane(new_lane(1, 10)).unwrap();
    assert!(!retry.created);
    assert_eq!(retry.lane, stopped.lane);
    h.start_lane_run("lane-002", 0, "root-2", 20).unwrap();
    assert!(
        !h.block_lane_start("lane-002", "cancel running", 21)
            .unwrap()
            .changed
    );
    assert_eq!(
        h.start_lane_run("lane-002", 1, "root-3", 22)
            .unwrap()
            .generation,
        2
    );
}

#[test]
fn a_brief_is_revised_from_the_revision_read_and_keeps_the_lane_policy() {
    let f = Fixture::new();
    let handle = f.handle();
    let lane = handle.create_lane(new_lane(1, 10)).unwrap().lane;

    let mut revised = lane.brief.clone();
    revised.constraints.push("no new dependencies".into());
    let update = handle
        .revise_lane_brief("lane-001", 1, revised.clone(), 20)
        .unwrap();
    assert!(update.changed);
    assert_eq!(update.lane.brief_revision, 2);
    assert_eq!(update.lane.brief, revised);

    assert_eq!(
        handle
            .revise_lane_brief("lane-001", 1, revised.clone(), 21)
            .unwrap_err(),
        StorageError::StaleRevision {
            kind: "brief",
            expected: 1,
            current: 2
        }
    );
    assert!(
        !handle
            .revise_lane_brief("lane-001", 2, revised.clone(), 22)
            .unwrap()
            .changed
    );
    revised.report_policy = LanePolicy::Summary;
    assert!(matches!(
        handle.revise_lane_brief("lane-001", 2, revised, 23).unwrap_err(),
        StorageError::LaneInvalid(ref e) if e.field == "report_policy"
    ));
}

#[test]
fn delivery_keys_register_once_per_management_revision() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();

    assert!(
        handle
            .register_lane_delivery_key("lane-001", 1, "ready", "artifact-1", 11)
            .unwrap()
    );
    assert!(
        !handle
            .register_lane_delivery_key("lane-001", 1, "ready", "artifact-1", 12)
            .unwrap()
    );
    assert!(
        handle
            .register_lane_delivery_key("lane-001", 1, "question", "q-1", 13)
            .unwrap()
    );
    assert!(matches!(
        handle
            .register_lane_delivery_key("lane-001", 2, "ready", "artifact-1", 14)
            .unwrap_err(),
        StorageError::StaleRevision {
            kind: "management",
            ..
        }
    ));
}

#[test]
fn lane_list_orders_by_latest_activity_then_lane_id_and_reads_never_reorder() {
    let f = Fixture::new();
    let handle = f.handle();
    // Activity times: 003 and 001 tie at 50, 002 is newest, 004 is oldest.
    for (n, at) in [(1, 50), (2, 90), (3, 50), (4, 10)] {
        handle.create_lane(new_lane(n, at)).unwrap();
    }

    let first = handle.lane_list(list_request(None)).unwrap();
    assert_eq!(
        ids(&first),
        ["lane-002", "lane-001", "lane-003", "lane-004"]
    );
    assert!(!first.has_more && first.next_cursor.is_none());

    // Repeated reads leave the order and the ledger revision alone.
    let again = handle.lane_list(list_request(None)).unwrap();
    assert_eq!(ids(&again), ids(&first));
    assert_eq!(again.snapshot_revision, first.snapshot_revision);

    // Real activity moves a lane; an older timestamp does not move it back.
    handle.touch_lane_activity("lane-004", 200).unwrap();
    assert_eq!(
        handle
            .touch_lane_activity("lane-004", 5)
            .unwrap()
            .last_activity_at_ns,
        200
    );
    handle
        .append_lane_report("lane-003", 1, report(LaneStatus::Working, "tick"), 300)
        .unwrap();
    let moved = handle.lane_list(list_request(None)).unwrap();
    assert_eq!(
        ids(&moved),
        ["lane-003", "lane-004", "lane-002", "lane-001"]
    );
    assert!(moved.snapshot_revision > first.snapshot_revision);

    // Other leads and the policy filter are respected.
    let mut summary = new_lane(5, 400);
    summary.brief.report_policy = LanePolicy::Summary;
    handle.create_lane(summary).unwrap();
    let mut filtered = list_request(None);
    filtered.policy = Some(LanePolicy::Summary);
    assert_eq!(ids(&handle.lane_list(filtered).unwrap()), ["lane-005"]);
    let mut other = list_request(None);
    other.lead_thread_id = "somebody-else".into();
    assert!(handle.lane_list(other).unwrap().items.is_empty());
}

#[test]
fn pages_hold_twenty_lanes_and_cover_every_lane_once_while_the_ledger_changes() {
    // Given: 45 lanes, with activity ties so that the lane ID tie-break matters.
    let f = Fixture::new();
    let handle = f.handle();
    for n in 1..=45 {
        handle
            .create_lane(new_lane(n, i64::from(n % 9) * 10))
            .unwrap();
    }
    let first = handle.lane_list(list_request(None)).unwrap();
    assert_eq!(first.items.len(), LANE_LIST_PAGE_MAX);
    assert!(first.has_more);
    let frozen_revision = first.snapshot_revision;
    let mut expected = ids(&first);
    let mut cursor = first.next_cursor.clone();

    // When: the ledger changes between pages: activity moves lanes up, a lane is
    // created and a report arrives.
    handle.touch_lane_activity("lane-045", 10_000).unwrap();
    handle.touch_lane_activity("lane-001", 20_000).unwrap();
    handle.create_lane(new_lane(46, 30_000)).unwrap();
    handle
        .append_lane_report("lane-040", 1, report(LaneStatus::Ready, "done"), 40_000)
        .unwrap();
    let fresh = ids(&handle.lane_list(list_request(None)).unwrap());
    assert_ne!(fresh[..5], expected[..5], "a new listing sees the changes");

    let mut remaining_order = vec![];
    let mut pages = 1;
    while let Some(token) = cursor.take() {
        let page = handle.lane_list(list_request(Some(token))).unwrap();
        assert_eq!(page.snapshot_revision, frozen_revision);
        assert!(page.items.len() <= LANE_LIST_PAGE_MAX);
        remaining_order.extend(ids(&page));
        cursor = page.next_cursor;
        pages += 1;
    }
    expected.extend(remaining_order);

    // Then: the listing is the original one: 45 lanes, none twice, none missing,
    // lane-046 (created later) not in it, in the order of the first page's snapshot.
    assert_eq!(pages, 3);
    assert_eq!(expected.len(), 45);
    assert_eq!(expected.iter().collect::<BTreeSet<_>>().len(), 45);
    assert!(!expected.contains(&"lane-046".to_owned()));
    let mut by_snapshot = expected.clone();
    by_snapshot.sort();
    let all: Vec<String> = (1..=45).map(|n| format!("lane-{n:03}")).collect();
    assert_eq!(by_snapshot, all);
    // Ordered by activity descending, ties by lane ID, as of the snapshot.
    let activity = |lane: &str| {
        let n: i64 = lane[5..].parse().unwrap();
        n % 9 * 10
    };
    for pair in expected.windows(2) {
        let (a, b) = (activity(&pair[0]), activity(&pair[1]));
        assert!(a > b || (a == b && pair[0] < pair[1]), "{pair:?}");
    }
}

#[test]
fn a_lane_the_lead_no_longer_manages_drops_out_of_later_pages() {
    let f = Fixture::new();
    let handle = f.handle();
    for n in 1..=25 {
        handle.create_lane(new_lane(n, i64::from(n))).unwrap();
    }
    let first = handle.lane_list(list_request(None)).unwrap();
    // Newest first: lane-025 .. lane-006 on the first page, lane-005 .. lane-001 next.
    assert_eq!(first.items[0].lane_id, "lane-025");
    handle
        .apply_lane_management_transition(
            storage::lane_management::LaneManagementRequest {
                operation_id: format!("transition:{}", 100),
                lane_id: ("lane-003").to_string(),
                expected_management_revision: 1,
                action: storage::lane_management::LaneManagementAction::Detach,
            },
            storage::lane_management::LaneManagementActor::User,
            None,
            100,
        )
        .unwrap();

    assert_eq!(
        handle
            .lane_list(list_request(first.next_cursor))
            .unwrap_err(),
        StorageError::LaneCursor(LaneCursorError::Stale)
    );
}

#[test]
fn a_cursor_is_stale_after_its_listing_is_evicted_and_bound_to_its_filter() {
    let f = Fixture::new();
    let handle = f.handle();
    for n in 1..=21 {
        handle.create_lane(new_lane(n, i64::from(n))).unwrap();
    }
    let first = handle.lane_list(list_request(None)).unwrap();
    let oldest_cursor = first.next_cursor.clone().unwrap();
    assert!(
        handle
            .lane_list(list_request(Some(oldest_cursor.clone())))
            .is_ok()
    );

    // A cursor of another filter, and one nobody issued, are refused.
    let mut other_filter = list_request(Some(oldest_cursor.clone()));
    other_filter.policy = Some(LanePolicy::Summary);
    assert_eq!(
        handle.lane_list(other_filter).unwrap_err(),
        StorageError::LaneCursor(LaneCursorError::FilterMismatch)
    );
    assert_eq!(
        handle
            .lane_list(list_request(Some("not-a-cursor".into())))
            .unwrap_err(),
        StorageError::LaneCursor(LaneCursorError::Malformed)
    );
    let mut foreign_lead = list_request(Some(oldest_cursor.clone()));
    foreign_lead.lead_thread_id = "somebody-else".into();
    assert!(matches!(
        handle.lane_list(foreign_lead).unwrap_err(),
        StorageError::LaneCursor(LaneCursorError::FilterMismatch)
    ));

    // When: newer listings push it out of the bounded store.
    let mut newest = None;
    for _ in 0..LANE_LIST_SNAPSHOTS_PER_LEAD {
        newest = handle.lane_list(list_request(None)).unwrap().next_cursor;
    }

    // Then: the evicted listing is stale_cursor, the newest still continues.
    let error = handle
        .lane_list(list_request(Some(oldest_cursor)))
        .unwrap_err();
    assert_eq!(error, StorageError::LaneCursor(LaneCursorError::Stale));
    assert!(error.to_string().contains("stale_cursor"));
    assert!(handle.lane_list(list_request(newest)).is_ok());
}

#[test]
fn a_page_stops_at_sixteen_kib_and_the_rest_follows_through_the_cursor() {
    // Given: 20 lanes whose latest reports are each large (about 3 KiB serialized).
    let f = Fixture::new();
    let handle = f.handle();
    for n in 1..=20 {
        handle.create_lane(new_lane(n, i64::from(n))).unwrap();
        let mut big = report(LaneStatus::Working, &"s".repeat(1024));
        big.artifact_refs = vec!["a".repeat(256); 6];
        big.question_refs = vec!["q".repeat(256)];
        big.reason_code = Some("r".repeat(64));
        handle
            .append_lane_report(&format!("lane-{n:03}"), 1, big, 1_000 + i64::from(n))
            .unwrap();
    }

    // When: the listing is read page by page.
    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    let mut cursor = None;
    loop {
        let page = handle.lane_list(list_request(cursor)).unwrap();
        assert!(!page.items.is_empty());
        sizes.push(page_bytes(&page));
        seen.extend(ids(&page));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    // Then: no whole page (items, cursor and fixed fields, serialized) passed the budget, several pages were needed and every lane
    // appeared exactly once.
    assert!(
        sizes.iter().all(|size| *size <= LANE_LIST_PAGE_MAX_BYTES),
        "{sizes:?}"
    );
    assert!(sizes.len() > 1, "{sizes:?}");
    assert_eq!(seen.len(), 20);
    assert_eq!(seen.iter().collect::<BTreeSet<_>>().len(), 20);
}

#[test]
fn a_deleted_thread_hides_its_lane_and_fences_every_write() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();
    handle.create_lane(new_lane(2, 11)).unwrap();

    // When: retention tombstones the lane's thread, and separately the lead's thread.
    let conn = f.sql();
    conn.execute(
        "INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) VALUES ('thread-001', 'f', 1)",
        [],
    )
    .unwrap();

    // Then: the lane is invisible and nothing can be written for it.
    assert!(handle.lane("lane-001").unwrap().is_none());
    assert!(handle.lane_by_thread("thread-001").unwrap().is_none());
    assert!(matches!(
        handle
            .append_lane_report("lane-001", 1, report(LaneStatus::Working, "late"), 20)
            .unwrap_err(),
        StorageError::LaneNotFound { .. }
    ));
    assert!(conn
        .execute(
            "INSERT INTO lane_reports(lane_id, report_seq, status, summary, artifact_refs_json, \
             needs_human, question_refs_json, producer, management_revision, brief_revision, \
             created_at_ns) VALUES ('lane-001', 1, 'working', 's', '[]', 0, '[]', 'harness', 1, 1, 1)",
            [],
        )
        .is_err());
    assert!(
        conn.execute(
            "UPDATE lanes SET updated_at_ns = 99 WHERE lane_id = 'lane-001'",
            []
        )
        .is_err()
    );
    assert_eq!(all_pages(&handle), vec!["lane-002"]);

    // A new lane cannot be opened under a deleted origin thread or run.
    conn.execute(
        "INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) VALUES ('dead-lead', 'g', 1)",
        [],
    )
    .unwrap();
    let mut under_dead_lead = new_lane(3, 30);
    under_dead_lead.origin_thread_id = "dead-lead".into();
    assert!(matches!(
        handle.create_lane(under_dead_lead).unwrap_err(),
        StorageError::Tombstoned { .. }
    ));
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('dead-run', 'x', 'h')",
        [],
    )
    .unwrap();
    let mut under_dead_run = new_lane(4, 31);
    under_dead_run.origin_run_id = "dead-run".into();
    assert!(matches!(
        handle.create_lane(under_dead_run).unwrap_err(),
        StorageError::Tombstoned { .. }
    ));
}

#[test]
fn events_describe_the_committed_state_and_a_replay_is_not_an_event() {
    let f = Fixture::new();
    let handle = f.handle();
    let created = handle.create_lane(new_lane(1, 10)).unwrap();
    assert!(created.created);

    assert_eq!(
        created.lane.created_event(),
        Ev::LaneCreated {
            lane_id: "lane-001".into(),
            thread_id: "thread-001".into(),
            origin_run_id: "lead-run".into(),
            origin_thread_id: "lead".into(),
            lead_thread_id: Some("lead".into()),
            policy: LanePolicy::Attached,
            management_revision: 1,
        }
    );
    let appended = handle
        .append_lane_report("lane-001", 1, report(LaneStatus::Ready, "done"), 11)
        .unwrap();
    assert_eq!(
        appended
            .lane
            .report_appended_event(&appended.report, Some("lane-run".into())),
        Ev::LaneReportAppended {
            lane_id: "lane-001".into(),
            thread_id: "thread-001".into(),
            run_id: Some("lane-run".into()),
            report_seq: 1,
            status: LaneStatus::Ready,
            needs_human: false,
        }
    );
    // A failed write returns an error, so there is nothing to publish.
    assert!(
        handle
            .append_lane_report("lane-001", 1, report(LaneStatus::Ready, ""), 12)
            .is_err()
    );
}

#[test]
fn a_report_is_limited_by_its_serialized_size_and_a_refusal_leaves_the_history_alone() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();
    let revision = handle
        .lane_list(list_request(None))
        .unwrap()
        .snapshot_revision;

    // Texts that each fit and sum to 3,840 bytes (below 4 KiB), but whose serialized
    // report is over: the limit is on the report, not on the sum of its texts.
    let mut by_sum = report(LaneStatus::Working, &"x".repeat(1024));
    by_sum.artifact_refs = vec!["y".repeat(256); 8];
    by_sum.question_refs = vec!["q".repeat(256); 3];
    // Escapes inflate a text once serialized: 2,048 quotes become 4,096 bytes.
    let mut escaped = report(LaneStatus::Working, &"\"".repeat(1024));
    escaped.question_refs = vec!["\"".repeat(256); 4];
    for oversized in [by_sum, escaped] {
        let error = handle
            .append_lane_report("lane-001", 1, oversized, 11)
            .unwrap_err();
        assert!(
            matches!(error, StorageError::LaneInvalid(ref e) if e.field == "report"),
            "{error:?}"
        );
    }

    // The refusals left no report, no sequence and no ledger change behind.
    assert!(
        f.database()
            .lane_reports("lane-001", 0, 50)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        handle.lane("lane-001").unwrap().unwrap().latest_report_seq,
        0
    );
    assert_eq!(
        handle
            .lane_list(list_request(None))
            .unwrap()
            .snapshot_revision,
        revision
    );

    // At the boundary: the longest report the ledger accepts serializes to at most
    // 4,096 bytes, and one more byte per question reference is refused.
    let mut accepted = 0_usize;
    let mut refused_at = None;
    for length in 1..=256 {
        let mut candidate = report(LaneStatus::Working, &"x".repeat(1024));
        candidate.artifact_refs = vec!["y".repeat(256); 8];
        candidate.question_refs = vec!["q".repeat(length); 3];
        match handle.append_lane_report("lane-001", 1, candidate, 12) {
            Ok(appended) => {
                accepted = serde_json::to_string(&appended.report).unwrap().len();
                assert!(accepted <= 4096, "{accepted} bytes accepted");
            }
            Err(error) => {
                assert!(matches!(error, StorageError::LaneInvalid(_)), "{error:?}");
                refused_at = Some(length);
                break;
            }
        }
    }
    assert!(refused_at.is_some(), "the limit must be reached");
    assert!(
        accepted + 3 > 4096,
        "the last accepted report was {accepted} bytes"
    );
}

#[test]
fn a_workspace_that_serializes_large_is_refused_so_a_list_entry_stays_small() {
    let f = Fixture::new();
    let handle = f.handle();
    let mut escaping = new_lane(1, 10);
    escaping.brief.workspace = LaneWorkspaceDeclaration {
        branch: Some("\u{1}".repeat(512)),
        worktree: Some("\u{1}".repeat(512)),
        notes: None,
    };
    assert!(matches!(
        handle.create_lane(escaping).unwrap_err(),
        StorageError::LaneInvalid(ref e) if e.field == "workspace"
    ));

    // A workspace of quotes (doubled by escaping) that fits is listed within the budget,
    // next to a report that is as large as the ledger allows.
    for n in 1..=20 {
        let mut lane = new_lane(n, i64::from(n));
        lane.brief.objective = "\"".repeat(600);
        lane.brief.workspace = LaneWorkspaceDeclaration {
            branch: Some("\"".repeat(512)),
            worktree: Some("\"".repeat(512)),
            notes: None,
        };
        handle.create_lane(lane).unwrap();
        let mut big = report(LaneStatus::Working, &"\"".repeat(400));
        big.artifact_refs = vec!["a".repeat(256); 6];
        big.question_refs = vec!["q".repeat(200)];
        handle
            .append_lane_report(&format!("lane-{n:03}"), 1, big, 1_000 + i64::from(n))
            .unwrap();
    }
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = handle.lane_list(list_request(cursor)).unwrap();
        assert!(
            page_bytes(&page) <= LANE_LIST_PAGE_MAX_BYTES,
            "{}",
            page_bytes(&page)
        );
        // The preview is cut by its serialized size, escapes included.
        assert!(page.items.iter().all(|item| {
            serde_json::to_string(&item.objective_preview)
                .unwrap()
                .len()
                <= 256 + 2
        }));
        seen.extend(ids(&page));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.iter().collect::<BTreeSet<_>>().len(), 20);
}

#[test]
fn a_write_that_fails_half_way_leaves_nothing_behind() {
    let f = Fixture::new();
    let handle = f.handle();
    handle.create_lane(new_lane(1, 10)).unwrap();
    let revision = handle
        .lane_list(list_request(None))
        .unwrap()
        .snapshot_revision;
    let conn = f.sql();

    // A report whose last step (moving the lane's latest sequence) fails.
    conn.execute_batch(
        "CREATE TRIGGER inject_report BEFORE UPDATE OF latest_report_seq ON lanes \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    assert!(
        handle
            .append_lane_report("lane-001", 1, report(LaneStatus::Working, "x"), 11)
            .is_err()
    );
    conn.execute_batch("DROP TRIGGER inject_report").unwrap();
    assert!(
        f.database()
            .lane_reports("lane-001", 0, 50)
            .unwrap()
            .is_empty()
    );

    // A creation whose lane row fails after its thread row was written.
    conn.execute_batch(
        "CREATE TRIGGER inject_create BEFORE INSERT ON lanes \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    assert!(handle.create_lane(new_lane(2, 12)).is_err());
    assert!(f.database().thread("thread-002").unwrap().is_none());
    assert!(handle.lane("lane-002").unwrap().is_none());
    conn.execute_batch("DROP TRIGGER inject_create").unwrap();

    // Nothing moved, and the same requests succeed once the fault is gone.
    assert_eq!(
        handle
            .lane_list(list_request(None))
            .unwrap()
            .snapshot_revision,
        revision
    );
    assert_eq!(
        handle
            .append_lane_report("lane-001", 1, report(LaneStatus::Working, "x"), 13)
            .unwrap()
            .report
            .report_seq,
        1
    );
    assert!(handle.create_lane(new_lane(2, 14)).unwrap().created);
}

#[test]
fn a_reopened_database_keeps_the_creation_and_its_unstarted_intent() {
    // Given: a lane created, and the process ends before its root started.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("lanes.db"),
        ..StorageConfig::default()
    };
    let created = {
        let storage = Storage::open(config.clone()).unwrap();
        let created = storage.handle().create_lane(new_lane(1, 10)).unwrap();
        storage.close();
        created
    };

    // When: it is opened again and the same creation is retried.
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let retried = handle.create_lane(new_lane(1, 99)).unwrap();

    // Then: the retry is the same lane, and the intent is found for the start.
    assert!(!retried.created);
    assert_eq!(retried.lane, created.lane);
    let pending = Database::open(&config)
        .unwrap()
        .unstarted_lanes(10)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].start_seed, "seed");
    let started = handle.start_lane_run("lane-001", 0, "root-1", 20).unwrap();
    assert!(started.is_started());
    assert!(
        Database::open(&config)
            .unwrap()
            .unstarted_lanes(10)
            .unwrap()
            .is_empty()
    );
    storage.close();
}

#[test]
fn an_operator_stopped_intent_is_not_recovered_and_only_it_can_be_resumed() {
    let f = Fixture::new();
    let h = f.handle();
    for n in 1..=3 {
        h.create_lane(new_lane(n, 10)).unwrap();
    }
    let stopped = h
        .block_lane_start("lane-001", storage::lanes::LANE_START_OPERATOR_STOP, 12)
        .unwrap();
    assert!(stopped.lane.is_operator_stopped());
    h.block_lane_start("lane-002", "cancelled before root start", 12)
        .unwrap();
    // A crash scan recovers neither blocked intent, also after a reopen.
    assert_eq!(
        f.database()
            .unstarted_lanes(10)
            .unwrap()
            .iter()
            .map(|l| l.lane_id.as_str())
            .collect::<Vec<_>>(),
        ["lane-003"]
    );
    // A cancelled or locally failed intent cannot be resumed.
    assert_eq!(
        h.resume_lane_start("lane-002", 13).unwrap_err(),
        StorageError::LaneConflict(LaneConflictReason::StartBlocked)
    );
    // An unblocked intent is unchanged.
    assert!(!h.resume_lane_start("lane-003", 13).unwrap().changed);
    // The stop is lifted once, durably, and the intent is recoverable again.
    let resumed = h.resume_lane_start("lane-001", 14).unwrap();
    assert!(resumed.changed);
    assert!(resumed.lane.start_blocked_reason.is_none());
    assert!(!h.resume_lane_start("lane-001", 15).unwrap().changed);
    assert_eq!(
        f.database()
            .unstarted_lanes(10)
            .unwrap()
            .iter()
            .map(|l| l.lane_id.as_str())
            .collect::<Vec<_>>(),
        ["lane-001", "lane-003"]
    );
    // A started lane ignores both a stop and a resume.
    h.start_lane_run("lane-003", 0, "root-3", 20).unwrap();
    assert!(
        !h.block_lane_start("lane-003", storage::lanes::LANE_START_OPERATOR_STOP, 21)
            .unwrap()
            .changed
    );
    assert!(!h.resume_lane_start("lane-003", 22).unwrap().changed);
}

#[test]
fn an_operator_stop_hardens_into_a_cancellation_but_a_cancellation_is_never_loosened() {
    let f = Fixture::new();
    let h = f.handle();
    for n in 1..=2 {
        h.create_lane(new_lane(n, 10)).unwrap();
    }
    let stop = storage::lanes::LANE_START_OPERATOR_STOP;

    // Stop, then cancel: the cancellation replaces the resumable reason, once.
    assert!(h.block_lane_start("lane-001", stop, 11).unwrap().changed);
    assert!(
        !h.block_lane_start("lane-001", stop, 12).unwrap().changed,
        "a repeated stop is unchanged"
    );
    let hardened = h
        .block_lane_start("lane-001", "cancelled before root start", 13)
        .unwrap();
    assert!(hardened.changed);
    assert!(!hardened.lane.is_operator_stopped());
    assert_eq!(
        h.resume_lane_start("lane-001", 14).unwrap_err(),
        StorageError::LaneConflict(LaneConflictReason::StartBlocked)
    );
    assert!(
        !h.block_lane_start("lane-001", "another failure", 15)
            .unwrap()
            .changed
    );

    // Cancel, then stop: the cancellation stays.
    h.block_lane_start("lane-002", "cancelled before root start", 11)
        .unwrap();
    let kept = h.block_lane_start("lane-002", stop, 12).unwrap();
    assert!(!kept.changed);
    assert_eq!(
        kept.lane.start_blocked_reason.as_deref(),
        Some("cancelled before root start")
    );
    assert_eq!(
        f.database()
            .lane("lane-001")
            .unwrap()
            .unwrap()
            .start_blocked_reason
            .as_deref(),
        Some("cancelled before root start")
    );
}

#[test]
fn policy_edit_invalidates_filtered_cursor_without_leaking_changed_member() {
    use storage::lane_management::*;
    let f = Fixture::new();
    let h = f.handle();
    for n in 1..=25 {
        h.create_lane(new_lane(n, i64::from(n))).unwrap();
    }
    let mut request = list_request(None);
    request.policy = Some(LanePolicy::Attached);
    let first = h.lane_list(request.clone()).unwrap();
    assert!(first.has_more);
    h.apply_lane_management_transition(
        LaneManagementRequest {
            operation_id: "policy-cursor".into(),
            lane_id: "lane-003".into(),
            expected_management_revision: 1,
            action: LaneManagementAction::SetPolicy {
                policy: ManagedLanePolicy::Summary,
            },
        },
        LaneManagementActor::User,
        None,
        30,
    )
    .unwrap();
    request.cursor = first.next_cursor;
    assert_eq!(
        h.lane_list(request.clone()).unwrap_err(),
        StorageError::LaneCursor(LaneCursorError::Stale)
    );
    request.cursor = None;
    let page = h.lane_list(request.clone()).unwrap();
    request.cursor = page.next_cursor;
    let second = h.lane_list(request).unwrap();
    assert!(!second.items.iter().any(|i| i.lane_id == "lane-003"));
    let mut summary = list_request(None);
    summary.policy = Some(LanePolicy::Summary);
    assert_eq!(ids(&h.lane_list(summary).unwrap()), vec!["lane-003"]);
}

#[test]
fn creation_lookup_and_internal_membership_are_read_only_and_policy_independent() {
    let fixture = Fixture::new();
    let handle = fixture.handle();
    let pending = handle.create_lane(new_lane(1, 1)).unwrap().lane;
    let mut detached = new_lane(2, 2);
    detached.brief.report_policy = LanePolicy::Detached;
    let detached = handle.create_lane(detached).unwrap().lane;
    assert_eq!(
        handle.lane_by_creation_key("lead", "call-001").unwrap(),
        Some(pending.clone())
    );
    assert_eq!(
        handle.lane_by_creation_key("lead", "call-002").unwrap(),
        Some(detached.clone())
    );
    assert!(
        handle
            .lane_by_creation_key("other", "call-001")
            .unwrap()
            .is_none()
    );
    assert!(
        handle
            .lane_by_creation_key("lead", "missing")
            .unwrap()
            .is_none()
    );
    let before: i64 = fixture
        .sql()
        .query_row("SELECT count(*) FROM lane_list_snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(handle.managed_lanes("lead").unwrap(), vec![pending.clone()]);
    assert!(handle.managed_lanes("other").unwrap().is_empty());
    assert_eq!(
        fixture.database().managed_lane_leads().unwrap(),
        vec![(pending.lane_id.clone(), "lead".to_owned())]
    );
    let after: i64 = fixture
        .sql()
        .query_row("SELECT count(*) FROM lane_list_snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(handle.lane(&pending.lane_id).unwrap(), Some(pending));
    assert_eq!(handle.lane(&detached.lane_id).unwrap(), Some(detached));
}
