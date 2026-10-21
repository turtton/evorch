//! Process-kill boundaries across the two databases (ADR 0037).
mod support;

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use event_bus::{AgentRunPhase, EventBus};
use runtime::ownership::{
    CheckpointBoundary as Boundary, Lease, OwnerCheckpoint, OwnerPermit, Registry,
    SnapshotReference as Reference, ThreadOwner,
};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunStore};
use storage::{Database, Storage, StorageConfig};

fn config(root: &std::path::Path) -> StorageConfig {
    StorageConfig {
        db_path: root.join("events.db"),
        ..Default::default()
    }
}
fn runtime(config: &StorageConfig, storage: &Storage) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(support::ScriptedModel::new([Ok(support::text_response(
            "answer",
            providers::FinishReason::Stop,
        ))])),
    )
    .with_run_store(RunStore::open(config, storage.handle()).unwrap())
}
fn permit(root: &std::path::Path, run: RunId) -> OwnerPermit {
    OwnerPermit {
        registry_path: root.join("owners.db"),
        thread_id: "thread".into(),
        lease: Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
        run_id: Some(run.to_string()),
    }
}
/// Kill AND reap on every exit path, including timeout/assertion panics.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn signal(root: &std::path::Path) -> ! {
    std::fs::write(root.join("ready"), "ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn checkpoint_crash_child() {
    let Some(root) = std::env::var_os("EVORCH_CHECKPOINT_CRASH_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let run: RunId = std::env::var("EVORCH_CHECKPOINT_CRASH_RUN")
        .unwrap()
        .parse()
        .unwrap();
    let stage = std::env::var("EVORCH_CHECKPOINT_CRASH_STAGE").unwrap();
    let permit = permit(&root, run);
    permit.begin_turn().unwrap();
    if stage == "before_snapshot" {
        signal(&root);
    }
    let config = config(&root);
    let store = Storage::open(config.clone()).unwrap();
    let mut record = Database::open(&config)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    let mut messages: Vec<providers::Message> =
        serde_json::from_str(&record.messages_json).unwrap();
    messages.push(providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: "durable-after-boundary".into(),
        }],
    });
    record.messages_json = serde_json::to_string(&messages).unwrap();
    record.updated_at_ns += 1;
    let receipt = store.handle().upsert_run_context_receipt(&record).unwrap();
    if stage == "after_snapshot" {
        signal(&root);
    }
    let snapshot = Reference::RunContext { receipt };
    if stage == "uncommitted_owner" {
        // Exact two-row transaction, held before commit. SIGKILL leaves the
        // rollback journal; reopening must restore BOTH authority and reference.
        let connection = rusqlite::Connection::open(&permit.registry_path).unwrap();
        connection
            .execute_batch("PRAGMA synchronous=FULL; BEGIN IMMEDIATE")
            .unwrap();
        let state: String = connection
            .query_row(
                "SELECT state FROM thread_owners WHERE thread_id='thread'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut owner: ThreadOwner = serde_json::from_str(&state).unwrap();
        owner
            .checkpoint_run(&permit.lease, &run.to_string())
            .unwrap();
        let metadata = serde_json::json!({"v":1,"generation":1,"boundary":"terminal","runs":{run.to_string():{"snapshot":snapshot,"boundary":"terminal","checkpointed_at_ms":1}}});
        connection
            .execute(
                "UPDATE owner_checkpoints SET payload=?1 WHERE thread_id='thread'",
                [metadata.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE thread_owners SET state=?1 WHERE thread_id='thread'",
                [serde_json::to_string(&owner).unwrap()],
            )
            .unwrap();
        connection.cache_flush().unwrap();
        assert!(root.join("owners.db-journal").exists());
        signal(&root);
    }
    permit
        .checkpoint_metadata(Boundary::Terminal, snapshot)
        .unwrap();
    signal(&root);
}

#[tokio::test]
async fn kill_at_each_boundary_recovers_history_and_allows_successor_claim() {
    for stage in [
        "before_snapshot",
        "after_snapshot",
        "uncommitted_owner",
        "after_commit",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let config = config(root);
        let store = Storage::open(config.clone()).unwrap();
        let first = runtime(&config, &store);
        let initial = permit(root, "run-1".parse().unwrap());
        Registry::open(&initial.registry_path)
            .unwrap()
            .start(&ThreadOwner::new("thread".into(), initial.lease.clone()))
            .unwrap();
        let run = first.delegate_background(
            Role::Worker,
            "original".into(),
            RunConfig {
                ownership: Some(OwnerPermit {
                    run_id: None,
                    ..initial.clone()
                }),
                ..Default::default()
            },
        );
        assert_eq!(first.wait(run).await.unwrap(), AgentRunPhase::Done);
        drop(first);
        store.close();
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "checkpoint_crash_child", "--nocapture"])
                .env("EVORCH_CHECKPOINT_CRASH_ROOT", root)
                .env("EVORCH_CHECKPOINT_CRASH_RUN", run.to_string())
                .env("EVORCH_CHECKPOINT_CRASH_STAGE", stage)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited at {stage}"
            );
            assert!(Instant::now() < deadline, "child timed out at {stage}");
            std::thread::sleep(Duration::from_millis(5));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let old = permit(root, run);
        let mut registry = Registry::open_existing(&old.registry_path).unwrap();
        let owner = registry.attach("thread").unwrap();
        assert_eq!(owner.active_turn, stage != "after_commit", "{stage}");
        let saved = Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.messages_json.contains("durable-after-boundary"),
            stage != "before_snapshot",
            "{stage}"
        );
        let payload: String = rusqlite::Connection::open(&old.registry_path)
            .unwrap()
            .query_row(
                "SELECT payload FROM owner_checkpoints WHERE thread_id='thread'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let checkpoint = OwnerCheckpoint::decode(&payload).unwrap();
        assert_eq!(
            checkpoint.runs[&run.to_string()]
                .snapshot
                .available(Some(&saved)),
            matches!(stage, "before_snapshot" | "after_commit")
        );
        let owner = registry
            .update("thread", |owner| {
                owner.lease.expires_at = 0;
                owner.claim(&old.lease, "successor", u64::MAX / 2, 0)
            })
            .unwrap();
        assert!(!owner.active_turn);
        assert!(owner.active_runs.is_empty());
        let successor = OwnerPermit {
            lease: owner.lease,
            run_id: None,
            ..old
        };
        let store = Storage::open(config.clone()).unwrap();
        let next = runtime(&config, &store);
        next.continue_goal(
            run,
            "new human request".into(),
            RunConfig {
                ownership: Some(successor),
                ..Default::default()
            },
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if next
                    .list_agents()
                    .iter()
                    .any(|agent| agent.run_id == run && agent.phase == AgentRunPhase::Waiting)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("continued history reaches input waiting");
        next.stop(run, runtime::StopScope::SelfOnly).unwrap();
        assert_eq!(
            next.wait(run).await.unwrap(),
            AgentRunPhase::Stopped,
            "{stage}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn child_guard_reaps_on_success_and_panic() {
    for panic in [false, true] {
        let child = ChildGuard(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.0.id();
        let result = std::panic::catch_unwind(move || {
            let _guard = child;
            assert!(!panic, "exercise crash-test panic cleanup");
        });
        assert_eq!(result.is_err(), panic);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
}
