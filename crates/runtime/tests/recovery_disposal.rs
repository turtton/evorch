//! Owner-free disposal through public APIs, the real writer and filesystem.
use runtime::runtime::recovery_disposal::DisposalReservation;
use runtime::{AgentRuntime, DisposalAuthority, Role, RunId, RunStore, RuntimeError};
use std::{path::PathBuf, sync::Arc};
use storage::recovery::{
    graph_control::{GraphControlCas, GraphEffectKey},
    graph_progress::{GraphDisposalProgressKey, GraphDisposalStep},
};
use storage::{Database, Storage, StorageConfig, recovery::restart::*};

const GRAPH: &str = "disposal-graph";
const TX: &str = "disposal-restart";
const THREAD: &str = "disposal-thread";
struct NeverModel;
#[async_trait::async_trait]
impl runtime::AgentModel for NeverModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "never".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        panic!("disposal must not execute a provider request")
    }
}
struct Fixture {
    runtime: AgentRuntime,
    scratch: Arc<runtime::scratch::ThreadScratch>,
    storage: Storage,
    config: StorageConfig,
    root: PathBuf,
    sidebar: PathBuf,
    leases: Arc<storage::WriterLeases>,
    _maintenance: storage::SharedLeases,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self::new_with_thread(true)
    }
    fn new_with_thread(has_thread: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let sidebar = dir.path().join("sidebar.json");
        std::fs::write(&sidebar, "{}").unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        drop(Database::open(&config).unwrap());
        let maintenance =
            storage::shared_with_wait(&config.db_path, &root, std::time::Duration::ZERO).unwrap();
        let leases = Arc::new(
            storage::WriterLeases::acquire(&config.db_path, &root, std::time::Duration::ZERO)
                .unwrap(),
        );
        let storage = Storage::open_with_writer_lease(
            config.clone(),
            maintenance.db_lease().unwrap(),
            leases.db_lease(),
        )
        .unwrap();
        let db = Database::open(&config).unwrap();
        assert_eq!(
            runtime::thread_retention::binding::ensure_binding(
                &db,
                &storage.handle(),
                &leases.authority().unwrap(),
                runtime::thread_retention::BindingPaths {
                    ownership_root: &root,
                    sidebar_path: &sidebar
                },
                &config.db_path,
                1,
            )
            .unwrap(),
            runtime::thread_retention::BindingStatus::Bound
        );
        let run = RunId::new(1).to_string();
        if has_thread {
            storage
                .handle()
                .upsert_thread_runtime(&storage::threads::ThreadUpsert {
                    thread_id: THREAD.into(),
                    root_run_id: Some(run.clone()),
                    ..Default::default()
                })
                .unwrap();
            rusqlite::Connection::open(&config.db_path)
            .unwrap()
            .execute(
                "INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,?2,?1,0)",
                rusqlite::params![run, THREAD],
            )
            .unwrap();
        }
        storage.handle().save_resume_intent(storage::resume_intent::ResumeIntentRecord {
            run_id: run.clone(), incarnation: "old".into(),
            payload: serde_json::json!({"run_id":run,"incarnation":"old","active":true,"stop_reason":null,"tool_intent":[],"initial_input":{"id":"initial"},"wait":{"kind":"admission_queued"}}).to_string(),
        }, None).unwrap();
        let metadata = RestartMetadata {
            restart_id: TX.into(),
            capture_id: "disposal-capture".into(),
            config_identity: "config".into(),
            old_epoch: 1,
            successor_epoch: 2,
            old_build: "old".into(),
            successor_build: "new".into(),
            target_identity: "target".into(),
            handoff_version: 1,
            snapshot_version: 1,
            storage_version: 81,
            old_builtin_tool_names: Default::default(),
            old_compiled_catalog_fingerprint: None,
            new_compiled_catalog_fingerprint: None,
            old_effective_tools_fingerprint: None,
            new_effective_tools_fingerprint: None,
        };
        let mut tx = storage
            .handle()
            .mutate_restart(TX.into(), None, RestartMutation::Create(Box::new(metadata)))
            .unwrap();
        for step in [
            RestartMutation::BeginCapture(vec![RestartExpectedMember {
                run_id: run.clone(),
                incarnation: "old".into(),
                root_run_id: run.clone(),
                thread_id: has_thread.then(|| THREAD.into()),
                intent_revision: 0,
                snapshot: None,
                lane_id: None,
            }]),
            RestartMutation::SaveComponent(storage::recovery::RecoveryManifestRecord {
                manifest_id: GRAPH.into(),
                schema_version: 2,
                committed: false,
                superseded: false,
                edges: vec![],
                members: vec![storage::recovery::RecoveryMemberRecord {
                    run_id: run,
                    incarnation: "old".into(),
                    intent_revision: 0,
                    snapshot: None,
                    resume: true,
                    record_json: "{}".into(),
                }],
            }),
            RestartMutation::Commit,
            RestartMutation::Arm {
                nonce_hash: "nonce".into(),
            },
            RestartMutation::Adopt {
                nonce_hash: "nonce".into(),
                successor_epoch: 2,
                config_identity: "config".into(),
                successor_build: "new".into(),
            },
            RestartMutation::Ready { at_ms: 1 },
        ] {
            tx = storage
                .handle()
                .mutate_restart(TX.into(), Some(tx.revision), step)
                .unwrap();
        }
        let scratch = Arc::new(runtime::scratch::ThreadScratch::at(
            dir.path().join("scratch"),
        ));
        let runtime = Self::runtime(&config, &storage, &leases, &root, &sidebar, &scratch);
        std::fs::write(scratch_dir(&scratch).join("before"), "before").unwrap();
        Self {
            runtime,
            scratch,
            storage,
            config,
            root,
            sidebar,
            leases,
            _maintenance: maintenance,
            _dir: dir,
        }
    }
    fn runtime(
        config: &StorageConfig,
        storage: &Storage,
        leases: &Arc<storage::WriterLeases>,
        root: &std::path::Path,
        sidebar: &std::path::Path,
        scratch: &Arc<runtime::scratch::ThreadScratch>,
    ) -> AgentRuntime {
        let bus = Arc::new(event_bus::EventBus::new(128));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(::tools::ToolExecutor::new(bus)),
            Arc::new(NeverModel),
        )
        .with_run_store(RunStore::open(config, storage.handle()).unwrap());
        runtime.set_thread_scratch(scratch.clone());
        runtime.set_disposal_context(leases.clone(), root.into(), sidebar.into());
        runtime
    }
    fn db(&self) -> Database {
        Database::open(&self.config).unwrap()
    }
    fn cas(&self) -> GraphControlCas {
        self.db().recovery_graph_cas(TX, GRAPH).unwrap()
    }
    fn reserve(&self, id: &str) -> (DisposalAuthority, GraphEffectKey, DisposalReservation) {
        let cas = self.cas();
        let effect = GraphEffectKey {
            operation_id: id.into(),
            attempt_nonce: format!("nonce-{id}"),
        };
        let reservation = self
            .runtime
            .reserve_owned_graph_disposal(GRAPH, effect.clone(), cas.control_revision)
            .unwrap();
        (
            self.runtime.disposal_authority(cas).unwrap(),
            effect,
            reservation,
        )
    }
    fn key(&self) -> GraphDisposalProgressKey {
        GraphDisposalProgressKey {
            manifest_id: GRAPH.into(),
            restart_id: TX.into(),
            capture_id: "disposal-capture".into(),
            thread_id: THREAD.into(),
        }
    }
}

#[tokio::test]
async fn revokes_before_reset_and_durable_progress_prevents_redeletion() {
    let f = Fixture::new();
    let (authority, effect, reservation) = f.reserve("release");
    let first = f
        .runtime
        .dispose_owned_graph(&authority, effect.clone(), &reservation)
        .unwrap();
    assert!(first.complete(), "{first:?}");
    assert_eq!(first.threads[0].scratch, GraphDisposalStep::Applied);
    assert!(
        f.db()
            .recovery_graph_control(GRAPH)
            .unwrap()
            .unwrap()
            .revoked
    );
    assert!(!f.scratch.thread_dir(THREAD).join("before").exists());
    std::fs::write(scratch_dir(&f.scratch).join("after"), "after").unwrap();
    let again = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(again.complete(), "{again:?}");
    assert_eq!(
        std::fs::read(f.scratch.thread_dir(THREAD).join("after")).unwrap(),
        b"after"
    );
    let binding = f.db().thread(THREAD).unwrap().unwrap();
    assert_eq!(binding.root_run_id, Some(RunId::new(1).to_string()));
}

#[tokio::test]
async fn foreign_runtime_and_stale_graph_cas_cannot_revoke() {
    let f = Fixture::new();
    let (authority, effect, reservation) = f.reserve("identity");
    let foreign = Fixture::runtime(
        &f.config, &f.storage, &f.leases, &f.root, &f.sidebar, &f.scratch,
    );
    assert!(
        foreign
            .dispose_owned_graph(&authority, effect.clone(), &reservation)
            .is_err()
    );
    assert!(f.db().recovery_graph_control(GRAPH).unwrap().is_none());
    let mut stale = f.cas();
    stale.capture_id.push_str("-stale");
    assert!(f.runtime.disposal_authority(stale).is_err());
    assert!(f.scratch.thread_dir(THREAD).join("before").exists());
}

#[tokio::test]
async fn binding_mismatch_retains_scratch_without_faking_completion() {
    let f = Fixture::new();
    let (authority, effect, reservation) = f.reserve("mismatch");
    std::fs::write(
        f.root
            .join(runtime::thread_retention::binding::ROOT_BINDING_FILE),
        "{}",
    )
    .unwrap();
    let result = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(!result.complete(), "{result:?}");
    assert!(result.unknown);
    assert!(result.control.unwrap().revoked);
    assert_eq!(result.threads[0].scratch, GraphDisposalStep::Unknown);
    assert!(f.scratch.thread_dir(THREAD).join("before").exists());
}

#[tokio::test]
async fn a_new_live_root_keeps_shared_scratch() {
    let f = Fixture::new();
    let (authority, effect, reservation) = f.reserve("shared");
    f.runtime.bind_thread_root(THREAD, RunId::new(90)).unwrap();
    let result = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(result.complete(), "{result:?}");
    assert_eq!(result.threads[0].scratch, GraphDisposalStep::NotApplicable);
    assert!(f.scratch.thread_dir(THREAD).join("before").exists());
}

#[tokio::test]
async fn crash_after_reset_before_progress_ack_retains_the_new_namespace() {
    let mut f = Fixture::new();
    let (authority, effect, reservation) = f.reserve("crashed");
    // The real disposal path commits Unknown, detaches scratch, then loses
    // the Applied write. Rebuilding memory here models a cold crash at that
    // uncertainty boundary without inventing a completed progress receipt.
    let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_crash_ack BEFORE UPDATE ON recovery_graph_thread_progress WHEN NEW.scratch='applied' BEGIN SELECT RAISE(FAIL,'lost applied acknowledgement'); END;").unwrap();
    let failed = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(!failed.complete());
    assert!(failed.unknown);
    assert!(!f.scratch.thread_dir(THREAD).join("before").exists());
    drop(authority);
    drop(reservation);
    sql.execute_batch("DROP TRIGGER fail_crash_ack").unwrap();
    // Reconstruct both runtime and scratch: no old reset key/epoch memory is
    // available, just as after a cold process restart.
    f.scratch = Arc::new(runtime::scratch::ThreadScratch::at(
        f.scratch.parent().into(),
    ));
    f.runtime = Fixture::runtime(
        &f.config, &f.storage, &f.leases, &f.root, &f.sidebar, &f.scratch,
    );
    std::fs::write(scratch_dir(&f.scratch).join("after-crash"), "keep").unwrap();
    let (authority, effect, reservation) = f.reserve("retry");
    let result = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(!result.complete(), "{result:?}");
    assert_eq!(result.threads[0].scratch, GraphDisposalStep::Unknown);
    assert_eq!(
        std::fs::read(f.scratch.thread_dir(THREAD).join("after-crash")).unwrap(),
        b"keep"
    );
}

#[tokio::test]
async fn unknown_correspondence_is_not_a_shared_or_completed_cleanup() {
    let f = Fixture::new();
    let (authority, effect, reservation) = f.reserve("unknown-users");
    // A different unreadable graph has no relational membership proof. Its
    // payload is irrelevant; the target's logical revocation still succeeds.
    rusqlite::Connection::open(&f.config.db_path).unwrap().execute(
        "INSERT INTO recovery_manifests(manifest_id,schema_version,committed,superseded,member_count,edge_count,edges_json) VALUES('unknown-graph',1,1,0,1,0,'[]')", [],
    ).unwrap();
    let result = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(!result.complete(), "{result:?}");
    assert!(result.unknown);
    assert!(result.control.unwrap().revoked);
    assert_eq!(result.threads[0].scratch, GraphDisposalStep::Unknown);
    assert!(f.scratch.thread_dir(THREAD).join("before").exists());
}

#[tokio::test]
async fn same_authority_retry_after_reset_write_failure_does_not_reset_again() {
    let f = Fixture::new();
    let (authority, effect, reservation) = f.reserve("lost-reset-ack");
    // The real writer refuses only the post-reset Applied transition. Unknown
    // was committed first, so this failure exercises the uncertainty window.
    let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_reset_ack BEFORE UPDATE ON recovery_graph_thread_progress WHEN NEW.scratch='applied' BEGIN SELECT RAISE(FAIL,'lost applied acknowledgement'); END;").unwrap();
    let failed = f
        .runtime
        .dispose_owned_graph(&authority, effect.clone(), &reservation)
        .unwrap();
    assert!(failed.unknown);
    assert!(!failed.complete());
    assert_eq!(
        f.db()
            .recovery_graph_thread_progress(&f.key())
            .unwrap()
            .unwrap()
            .scratch,
        GraphDisposalStep::Unknown
    );
    assert!(!f.scratch.thread_dir(THREAD).join("before").exists());
    std::fs::write(scratch_dir(&f.scratch).join("new"), "keep").unwrap();
    sql.execute_batch("DROP TRIGGER fail_reset_ack").unwrap();
    let retried = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(retried.complete(), "{retried:?}");
    assert_eq!(
        std::fs::read(f.scratch.thread_dir(THREAD).join("new")).unwrap(),
        b"keep"
    );
}

#[tokio::test]
async fn relationally_threadless_graph_revokes_without_touching_scratch() {
    let f = Fixture::new_with_thread(false);
    let (authority, effect, reservation) = f.reserve("threadless");
    assert!(f.cas().membership.iter().all(|m| m.thread_id.is_none()));
    let result = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(result.complete(), "{result:?}");
    assert!(result.threads.is_empty());
    assert!(!result.unknown);
    assert!(f.scratch.thread_dir(THREAD).join("before").exists());
}

#[tokio::test]
async fn empty_advertised_threads_are_not_a_proof_of_no_physical_binding() {
    let f = Fixture::new_with_thread(false);
    // The graph says threadless, but a current relational binding proves that
    // its physical correspondence is incomplete, not that cleanup is done.
    f.storage
        .handle()
        .upsert_thread_runtime(&storage::threads::ThreadUpsert {
            thread_id: THREAD.into(),
            root_run_id: Some(RunId::new(1).to_string()),
            ..Default::default()
        })
        .unwrap();
    let (authority, effect, reservation) = f.reserve("unknown-empty");
    assert!(f.cas().membership.iter().all(|m| m.thread_id.is_none()));
    let result = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(!result.complete(), "{result:?}");
    assert!(result.unknown);
    assert!(result.control.unwrap().revoked);
    assert!(f.scratch.thread_dir(THREAD).join("before").exists());
}

// Prepare the same real unmanaged scratch namespace without reaching into the
// runtime's private usage/reset bookkeeping.
fn scratch_dir(scratch: &runtime::scratch::ThreadScratch) -> PathBuf {
    sandbox::ScratchRoot::at(scratch.parent().into())
        .prepare(THREAD)
        .unwrap()
}

#[tokio::test]
async fn absent_writer_composition_revokes_but_does_not_authorize_scratch() {
    let mut f = Fixture::new();
    let bus = Arc::new(event_bus::EventBus::new(128));
    f.runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(::tools::ToolExecutor::new(bus)),
        Arc::new(NeverModel),
    )
    .with_run_store(RunStore::open(&f.config, f.storage.handle()).unwrap());
    f.runtime.set_thread_scratch(f.scratch.clone());
    // A writer handle alone is sufficient for logical revocation, never a
    // substitute for the existing physical writer/root/binding authority.
    let (authority, effect, reservation) = f.reserve("missing-writer-context");
    let result = f
        .runtime
        .dispose_owned_graph(&authority, effect, &reservation)
        .unwrap();
    assert!(result.control.as_ref().unwrap().revoked);
    assert!(result.unknown);
    assert!(!result.complete());
    assert_eq!(result.threads[0].scratch, GraphDisposalStep::Unknown);
    assert!(f.scratch.thread_dir(THREAD).join("before").exists());
}
