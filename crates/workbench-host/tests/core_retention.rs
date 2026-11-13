//! The core confirms the deletion of expired archived threads at start, with no
//! client involved, and keeps what 06b recovery depends on: a member of a
//! committed, not superseded recovery manifest protects the whole component
//! (a pin of one member holds the other), and a prepared claim blocks it. These
//! tests drive `acquire_leases` (exclusive phase, then shared leases) over a real
//! database, ownership root and sidebar file.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use storage::recovery::{
    RecoveryClaimRequest, RecoveryEdge, RecoveryManifestRecord, RecoveryMemberRecord,
};
use storage::resume_intent::ResumeIntentRecord;
use storage::thread_retention::ThreadRetentionPolicy;
use storage::{RunContextRecord, Storage, StorageConfig};
use workbench_host::bootstrap::retention::{RetentionPlan, acquire_leases};
use workspace_state::{ProjectId, SidebarDocument, SidebarStore, ThreadId, ThreadRecord};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    None,
    Committed,
    Prepared,
    Superseded,
}

struct World {
    _dir: tempfile::TempDir,
    plan: RetentionPlan,
}

fn run_record(id: &str) -> RunContextRecord {
    RunContextRecord {
        run_id: id.into(),
        role: "Worker".into(),
        name: id.into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: json!([{"role":"user","content":[]}]).to_string(),
        checkpoints_json: "[]".into(),
        terminal_phase: "running".into(),
        restorable: false,
        updated_at_ns: 1,
        host_inputs_json: "[]".into(),
    }
}

fn seed_manifest(storage: &Storage, state: Recovery) {
    let handle = storage.handle();
    let mut members = Vec::new();
    for id in ["run-a", "run-b"] {
        let record = run_record(id);
        let receipt = handle.upsert_run_context_receipt(&record).unwrap();
        let value = json!({
            "run_id": id, "incarnation": "old", "parent_run_id": null, "role": "Worker",
            "active": true, "stop_reason": Value::Null, "tool_intent": [],
            "snapshot": receipt, "provider_input": {"prefix": ["sent"]},
            "continuation": {"call_order": ["first", "second"]}
        });
        handle
            .save_resume_intent(
                ResumeIntentRecord {
                    run_id: id.into(),
                    incarnation: "old".into(),
                    payload: value.to_string(),
                },
                None,
            )
            .unwrap();
        members.push(RecoveryMemberRecord {
            run_id: id.into(),
            incarnation: "old".into(),
            intent_revision: 0,
            snapshot: Some(receipt),
            resume: true,
            record_json:
                json!({"run_id": id, "old_incarnation": "old", "parent": null, "role": "Worker"})
                    .to_string(),
        });
    }
    handle
        .prepare_recovery_manifest(RecoveryManifestRecord {
            manifest_id: "component".into(),
            schema_version: 1,
            committed: false,
            superseded: false,
            edges: vec![RecoveryEdge {
                from: "run-a".into(),
                to: "run-b".into(),
                kind: "wait".into(),
            }],
            members,
        })
        .unwrap();
    handle.commit_recovery_manifest("component".into()).unwrap();
    if state == Recovery::Prepared {
        handle
            .claim_recovery_manifest(RecoveryClaimRequest {
                claim_id: "claim".into(),
                manifest_id: "component".into(),
                incarnations: ["run-a", "run-b"]
                    .into_iter()
                    .map(|id| (id.to_owned(), format!("new-{id}")))
                    .collect(),
            })
            .unwrap();
    }
}

fn write_sidebar(path: &Path, repo: &Path, pin_a: bool) {
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(ProjectId::new("p1"), "p", repo)
        .unwrap();
    for id in ["A", "B"] {
        let mut thread = ThreadRecord::new(ThreadId::new(id), ProjectId::new("p1"), id);
        thread.created_at = 1;
        thread.updated_at = 1;
        document.sidebar.threads.push(thread);
    }
    document.sidebar.refresh_derived();
    for id in ["A", "B"] {
        document
            .sidebar
            .set_family_archived(&ThreadId::new(id), true, 1_000)
            .unwrap();
    }
    if pin_a {
        document
            .sidebar
            .set_family_pinned(&ThreadId::new("A"), true)
            .unwrap();
    }
    let store = SidebarStore::new(path);
    let lock = store.lock().unwrap();
    store.write(&lock, &document).unwrap();
}

fn world(state: Recovery, pin_a: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("project");
    let owners = dir.path().join("owners");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&owners).unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("evorch-events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    if state == Recovery::None {
        for id in ["run-a", "run-b"] {
            storage
                .handle()
                .upsert_run_context_receipt(&run_record(id))
                .unwrap();
        }
    } else {
        seed_manifest(&storage, state);
    }
    storage.close();
    let connection = rusqlite::Connection::open(&config.db_path).unwrap();
    if state == Recovery::Superseded {
        connection
            .execute("UPDATE recovery_manifests SET superseded = 1", [])
            .unwrap();
    }
    connection
        .execute_batch(
            "INSERT INTO threads(thread_id,project_root,root_run_id,archived,created_at_ns,updated_at_ns,source)
             VALUES ('A','/project','run-a',1,1,1,'host'),('B','/project','run-b',1,1,1,'host');
             INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth)
             VALUES ('run-a','A','run-a',0),('run-b','B','run-b',0);
             UPDATE run_contexts SET updated_at_ns = 1",
        )
        .unwrap();
    let sidebar_path = dir.path().join("sidebar.json");
    write_sidebar(&sidebar_path, &repo, pin_a);
    World {
        plan: RetentionPlan {
            db_path: config.db_path,
            ownership_root: owners,
            sidebar_path,
            artifacts_dir: None,
            policy: ThreadRetentionPolicy::default(),
        },
        _dir: dir,
    }
}

fn tombstones(plan: &RetentionPlan) -> Vec<String> {
    let connection = rusqlite::Connection::open(&plan.db_path).unwrap();
    let mut statement = connection
        .prepare("SELECT run_id FROM run_tombstones ORDER BY run_id")
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn run_core_start(plan: &RetentionPlan) {
    let leases = acquire_leases(plan, Duration::ZERO).unwrap();
    drop(leases);
}

#[test]
fn an_expired_archived_family_is_deleted_by_the_core_start_alone() {
    let world = world(Recovery::None, false);
    run_core_start(&world.plan);
    assert_eq!(tombstones(&world.plan), ["run-a", "run-b"]);
}

#[test]
fn a_pin_of_one_member_of_a_committed_component_holds_the_whole_component() {
    let world = world(Recovery::Committed, true);
    run_core_start(&world.plan);
    assert!(tombstones(&world.plan).is_empty());
}

#[test]
fn a_committed_component_is_deleted_whole_or_not_at_all() {
    let world = world(Recovery::Committed, false);
    run_core_start(&world.plan);
    assert_eq!(tombstones(&world.plan), ["run-a", "run-b"]);
}

#[test]
fn a_prepared_claim_blocks_the_deletion() {
    let world = world(Recovery::Prepared, false);
    run_core_start(&world.plan);
    assert!(tombstones(&world.plan).is_empty());
}

#[test]
fn a_superseded_manifest_no_longer_protects_its_members() {
    let world = world(Recovery::Superseded, true);
    run_core_start(&world.plan);
    let deleted = tombstones(&world.plan);
    assert!(
        !deleted.contains(&"run-a".to_owned()),
        "the pinned member stays: {deleted:?}"
    );
    assert_eq!(deleted, ["run-b"]);
}

#[test]
fn a_core_that_does_not_hold_the_exclusive_leases_deletes_nothing() {
    let world = world(Recovery::None, false);
    let other = storage::shared_with_wait(
        &world.plan.db_path,
        &world.plan.ownership_root,
        Duration::ZERO,
    )
    .unwrap();
    let leases = acquire_leases(&world.plan, Duration::ZERO).unwrap();
    drop((leases, other));
    assert!(tombstones(&world.plan).is_empty());
}

#[test]
fn without_a_sidebar_file_nothing_is_deleted() {
    let world = world(Recovery::None, false);
    std::fs::remove_file(&world.plan.sidebar_path).unwrap();
    run_core_start(&world.plan);
    assert!(tombstones(&world.plan).is_empty());
}
