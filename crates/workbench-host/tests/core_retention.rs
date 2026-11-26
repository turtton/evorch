//! Readiness must not perform an all-candidate deletion-confirmation pass.
//! New confirmations belong to the activated online retention owner.

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use storage::thread_retention::ThreadRetentionPolicy;
use storage::{RunContextRecord, Storage, StorageConfig};
use workbench_host::bootstrap::retention::{RetentionPlan, acquire_leases};
use workspace_state::{ProjectId, SidebarDocument, SidebarStore, ThreadId, ThreadRecord};

struct World {
    _dir: test_socket_dir::SplitDir,
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

fn write_sidebar(path: &Path, repo: &Path) {
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
    let store = SidebarStore::new(path);
    let lock = store.lock().unwrap();
    store.write(&lock, &document).unwrap();
}

fn world() -> World {
    let dir = test_socket_dir::SplitDir::new();
    let repo = dir.path().join("project");
    let owners = dir.sockets().to_path_buf();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&owners).unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("evorch-events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    for id in ["run-a", "run-b"] {
        storage
            .handle()
            .upsert_run_context_receipt(&run_record(id))
            .unwrap();
    }
    storage.close();
    let connection = rusqlite::Connection::open(&config.db_path).unwrap();
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
    write_sidebar(&sidebar_path, &repo);
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
fn readiness_does_not_confirm_expired_archived_families() {
    let world = world();
    run_core_start(&world.plan);
    assert!(tombstones(&world.plan).is_empty());
    // Activation's bounded online tick owns new confirmations, not readiness.
    // Existing jobs/crash recovery are exercised by runtime's startup suite.
}
