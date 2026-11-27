//! Real core bootstrap/SQLite/ownership. No GUI, provider, gh or wall-clock waits.
use event_bus::{Event, EventKind, EventReceiver, PullRequestEvent, ToolEvent};
use runtime::pull_requests::*;
use runtime::{AgentInvocationContext, AgentModel, Role, RunId, RuntimeError};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use storage::pull_requests::*;
use storage::{Storage, StorageConfig};
use tokio::sync::{Barrier, Notify};
use workbench_host::bootstrap::{
    BootstrapOptions, BootstrappedHost, TestSeams, lane_pr::PullRequestAdapters,
};

#[derive(Default)]
struct Model {
    entered: Notify,
    calls: AtomicUsize,
    hold_admission: AtomicBool,
    admission: Notify,
    admission_barrier: std::sync::Mutex<Option<Arc<Barrier>>>,
    release_admission: Notify,
    admitting: AtomicBool,
    skip_admission: AtomicBool,
    admissions_requested: AtomicUsize,
}
#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        std::future::pending().await
    }
    fn requires_admission(&self) -> bool {
        self.admissions_requested.fetch_add(1, Ordering::SeqCst);
        !self.skip_admission.load(Ordering::SeqCst)
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        if self.hold_admission.load(Ordering::SeqCst) {
            self.admitting.store(true, Ordering::SeqCst);
            let _active = Active(&self.admitting);
            let barrier = self.admission_barrier.lock().unwrap().take();
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
            self.admission.notify_one();
            self.release_admission.notified().await;
        }
        Ok(())
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "background-services".into()
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
#[derive(Default)]
struct Logs {
    entered: Notify,
    active: AtomicBool,
    calls: AtomicUsize,
}
struct Active<'a>(&'a AtomicBool);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl PullRequestLogSource for Logs {
    async fn fetch_failed_logs(&self, _: &LogRequest) -> CiLogBundle {
        self.active.store(true, Ordering::SeqCst);
        let _active = Active(&self.active);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        std::future::pending().await
    }
}
struct Fixture {
    dir: test_socket_dir::SplitDir,
    root: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        // Both executors bind an owner socket below this root.
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("config.toml"),
            "[pull_requests]\nenabled = true\n",
        )
        .unwrap();
        let storage = Storage::open(StorageConfig {
            db_path: root.join("evorch-events.db"),
            ..Default::default()
        })
        .unwrap();
        storage.close();
        Self { dir, root }
    }
    fn seed(&self) {
        self.seed_with_ownership(true);
    }
    fn seed_with_ownership(&self, requires_ownership: bool) {
        let storage = self.seed_storage();
        storage.handle().create_lane(storage::lanes::NewLane {
            lane_id: "recover".into(), thread_id: "lane-thread".into(), origin_thread_id: "lead".into(),
            origin_run_id: "run-41".into(), creation_key: "before-crash".into(),
            brief: serde_json::from_value(serde_json::json!({
                "objective":"resume durable intent", "inputs":[], "deliverables":["patch"],
                "acceptance_criteria":["review"], "constraints":[], "workspace":{}, "report_policy":"attached"
            })).unwrap(),
            start_seed: serde_json::json!({"run_id":"run-42", "project_root":self.dir.path().canonicalize().unwrap(), "requires_ownership":requires_ownership}).to_string(),
            project_id: None, project_root: Some(self.dir.path().canonicalize().unwrap().to_string_lossy().into_owned()), title: None, at_ns: 1,
        }).unwrap();
    }
    async fn core(
        &self,
        model: Arc<Model>,
        source: Arc<FakePullRequestSource>,
        logs: Arc<Logs>,
    ) -> BootstrappedHost {
        BootstrappedHost::open_with_service_seams(
            BootstrapOptions {
                config_dir: self.root.clone(),
                ownership_root: self.dir.sockets().to_path_buf(),
                project_root: self.dir.path().canonicalize().unwrap(),
            },
            TestSeams {
                model,
                sandbox: Arc::new(Direct),
            },
            PullRequestAdapters { source, logs },
        )
        .await
        .unwrap()
    }
    fn seed_storage(&self) -> Storage {
        Storage::open(StorageConfig {
            db_path: self.root.join("evorch-events.db"),
            ..Default::default()
        })
        .unwrap()
    }

    fn seed_broken_after_first(&self) {
        let storage = self.seed_storage();
        let lane = storage
            .handle()
            .lane_by_thread("lane-thread")
            .unwrap()
            .unwrap();
        storage
            .handle()
            .create_lane(storage::lanes::NewLane {
                lane_id: "zz-broken".into(),
                thread_id: "zz-broken-thread".into(),
                origin_thread_id: lane.origin_thread_id,
                origin_run_id: lane.origin_run_id,
                creation_key: "second-before-crash".into(),
                brief: lane.brief,
                start_seed: "not a start seed".into(),
                project_id: None,
                project_root: Some(
                    self.dir
                        .path()
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ),
                title: None,
                at_ns: 2,
            })
            .unwrap();
    }
}
fn key() -> PullRequestKey {
    PullRequestKey::github("owner/repo", 7).unwrap()
}
fn fetched(head: &str) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(),
        canonical: key(),
        url: "https://github.com/owner/repo/pull/7".into(),
        display: Default::default(),
        fetched_at_ns: 1,
        facts: PullRequestFacts {
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_repository: Some("owner/repo".into()),
            head_branch: "feature".into(),
            head_sha: head.into(),
            checks: vec![PullRequestCheck {
                provider_id: "ci".into(),
                kind: CheckKind::CheckRun,
                name: "ci".into(),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Failure),
                head_sha: head.into(),
            }],
            checks_complete: true,
            review_decision: ReviewDecision::None,
        },
    }
}
async fn candidate(events: &mut EventReceiver) {
    loop {
        if matches!(
            events.recv().await.unwrap().kind,
            EventKind::PullRequest(PullRequestEvent::CandidateChanged { .. })
        ) {
            break;
        }
    }
}
fn published(bus: &event_bus::EventBus) {
    bus.emit(Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "push".into(),
        input: Some(serde_json::json!({"command":"git push"})),
        run_id: Some("run-42".into()),
        parent_call_id: None,
    }));
    bus.emit(Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "push".into(),
        is_error: false,
        output: None,
        detail: None,
        run_id: Some("run-42".into()),
        parent_call_id: None,
    }));
    // The observer handles events in order. This durable candidate is a barrier
    // after the publishing success, rather than an executor yield or a sleep.
    bus.emit(Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "marker".into(),
        is_error: false,
        output: Some("https://github.com/owner/repo/pull/8".into()),
        detail: None,
        run_id: Some("run-42".into()),
        parent_call_id: None,
    }));
}
fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn checkout(root: &Path) -> String {
    git(root, &["init", "-b", "feature"]);
    // This disposable local fixture is not a repository change or a published commit.
    git(
        root,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn core_alone_recovers_before_ready_and_runs_monitor_autofix_then_drains() {
    let f = Fixture::new();
    let head = checkout(f.dir.path());
    f.seed();
    let model = Arc::new(Model::default());
    let source = Arc::new(FakePullRequestSource::new());
    let gate = source.push_gate(&key());
    let logs = Arc::new(Logs::default());
    let mut core = f.core(model.clone(), source.clone(), logs.clone()).await;
    // Initial recovery already acquired authority before the ready host escaped.
    let permit = core.owner.owned_permit("lane-thread").unwrap();
    model.entered.notified().await;
    assert_eq!(
        core.runtime.inspect_agent(RunId::new(42)).unwrap().run_id,
        RunId::new(42)
    );
    let service = core.runtime.pull_requests().unwrap();
    let mut events = core.runtime.event_bus().subscribe();
    service
        .link("lane-thread", "owner/repo#7", 0, &permit)
        .await
        .unwrap();
    gate.arrived().await;
    published(&core.runtime.event_bus());
    candidate(&mut events).await;
    let policy = core.storage_writer().pr_auto_fix_state(key()).unwrap();
    service
        .set_auto_fix("lane-thread", 1, policy.policy_revision, true, &permit)
        .await
        .unwrap();
    gate.release_ok(fetched(&head));
    logs.entered.notified().await;
    let notices = core
        .storage_writer()
        .pr_notices(key(), String::new())
        .unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].mode,
        storage::pull_request_auto_fix::PrNoticeMode::Auto
    );
    assert_eq!(source.calls().len(), 1);
    assert!(logs.active.load(Ordering::SeqCst));
    let runtime = core.runtime.clone();
    let (terminal, drained) = tokio::sync::oneshot::channel();
    core.services
        .start("terminal-dependent", |stop| async move {
            stop.await?;
            runtime.wait(RunId::new(42)).await?;
            terminal.send(()).unwrap();
            Ok(())
        });
    core.shutdown().await.unwrap();
    drained.await.unwrap();
    assert!(!logs.active.load(Ordering::SeqCst));
    assert_eq!(core.services.names().count(), 0);
    assert!(matches!(
        service.refresh("lane-thread", &permit).await,
        Err(PullRequestError::Disabled)
    ));
    assert_eq!(
        core.owner.attach("lane-thread").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
}

#[tokio::test]
async fn a_second_core_is_rejected_while_the_first_has_a_pending_lane_and_drain_cancels_it() {
    for requires_ownership in [true, false] {
        let f = Fixture::new();
        f.seed_with_ownership(requires_ownership);
        let model = Arc::new(Model::default());
        model.hold_admission.store(true, Ordering::SeqCst);
        let mut core = f
            .core(
                model.clone(),
                Arc::new(FakePullRequestSource::new()),
                Arc::new(Logs::default()),
            )
            .await;
        model.admission.notified().await;
        assert_eq!(core.storage_writer().unstarted_lanes(10).unwrap().len(), 1);
        let other = Arc::new(Model::default());
        let result = BootstrappedHost::open_with_service_seams(
            BootstrapOptions {
                config_dir: f.root.clone(),
                ownership_root: f.dir.sockets().into(),
                project_root: f.dir.path().into(),
            },
            TestSeams {
                model: other.clone(),
                sandbox: Arc::new(Direct),
            },
            PullRequestAdapters {
                source: Arc::new(FakePullRequestSource::new()),
                logs: Arc::new(Logs::default()),
            },
        )
        .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("DB writer lease unavailable")
        );
        assert_eq!(other.calls.load(Ordering::SeqCst), 0);
        assert!(core.busy(), "pending admission retains the core");
        assert!(model.admitting.load(Ordering::SeqCst));
        core.shutdown().await.unwrap();
        assert!(!model.admitting.load(Ordering::SeqCst));
        assert!(core.runtime.pending_lane_admissions().is_empty());
        model.release_admission.notify_one();
        assert!(core.runtime.list_agents().is_empty());
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn an_unowned_seed_keeps_the_handoff_fence_while_admission_is_pending() {
    unowned_seed_handoff(true, false, false).await;
}

#[tokio::test]
async fn an_unowned_seed_cancels_pending_admission_after_handoff() {
    unowned_seed_handoff(false, false, false).await;
}

#[tokio::test]
async fn an_unowned_seed_stops_only_its_admission_after_handoff() {
    unowned_seed_handoff(false, true, false).await;
}

#[tokio::test]
async fn manual_and_background_lane_retries_share_the_handoff_fence() {
    unowned_seed_handoff(true, false, true).await;
}

async fn unowned_seed_handoff(finish_admission: bool, stop_first: bool, manual_retry: bool) {
    let f = Fixture::new();
    f.seed_with_ownership(false);
    let store = f.seed_storage();
    let writer = store.handle();
    let old_admission = Arc::new(Barrier::new(2));
    let old_model = Arc::new(Model {
        hold_admission: AtomicBool::new(true),
        admission_barrier: std::sync::Mutex::new(Some(old_admission.clone())),
        ..Default::default()
    });
    let mut old = f.logical_runtime(old_model.clone(), writer.clone());
    old_admission.wait().await;
    assert!(old_model.admitting.load(Ordering::SeqCst));
    let next_admission = Arc::new(Barrier::new(2));
    let next_model = Arc::new(Model {
        hold_admission: AtomicBool::new(true),
        admission_barrier: std::sync::Mutex::new(Some(next_admission.clone())),
        ..Default::default()
    });
    let mut next = f.logical_runtime(next_model.clone(), writer.clone());
    let permit = old.owner.owned_permit("lane-thread").unwrap();
    old.owner.handoff(&permit, &next.owner).unwrap();
    if finish_admission {
        old_model.release_admission.notify_one();
        // Provider completion is not authority to register after handoff.
        assert!(matches!(
            old.runtime.wait(RunId::new(42)).await,
            Err(RuntimeError::StaleOwnership { .. })
        ));
    }
    assert!(old.runtime.list_agents().is_empty());
    assert_eq!(old_model.calls.load(Ordering::SeqCst), 0);
    // Admission entry proves the successor reservation exists. Waiting on the
    // run before this barrier raced background retry and could return UnknownRun.
    next_admission.wait().await;
    assert!(next_model.admitting.load(Ordering::SeqCst));
    if manual_retry {
        next.runtime
            .recover_unstarted_lanes(Some(&next.owner))
            .unwrap();
    }
    next_model.release_admission.notify_one();
    next_model.entered.notified().await;
    assert_eq!(next_model.calls.load(Ordering::SeqCst), 1);
    if stop_first {
        // The old owner can stop its local admission, not the successor's lane.
        assert!(matches!(
            old.runtime
                .stop(RunId::new(42), runtime::StopScope::SelfOnly),
            Err(RuntimeError::LaneCancellation { .. })
        ));
    }
    let shutdown = old.shutdown().await;
    if stop_first {
        assert!(matches!(
            shutdown.unwrap_err().downcast_ref::<RuntimeError>(),
            Some(RuntimeError::LaneCancellation { .. })
        ));
        old.runtime.drain_lane_admissions().await.unwrap();
        assert_eq!(
            old.runtime.quiesce_state(),
            runtime::quiesce::QuiesceState::Running
        );
    } else {
        shutdown.unwrap();
    }
    assert!(!old_model.admitting.load(Ordering::SeqCst));
    assert!(old.runtime.pending_lane_admissions().is_empty());
    assert!(old.runtime.list_agents().is_empty());
    assert_eq!(old_model.calls.load(Ordering::SeqCst), 0);
    if !finish_admission {
        assert!(old.runtime.wait(RunId::new(42)).await.is_err());
    }
    next.shutdown().await.unwrap();
    let lane = writer.lane_by_thread("lane-thread").unwrap().unwrap();
    assert_eq!(lane.generation, 1);
    assert!(writer.unstarted_lanes(10).unwrap().is_empty());
    drop((old, next));
    store.close();
    let restarted_model = Arc::new(Model::default());
    // A new core uses the graph-aware bootstrap; already-started legacy seeds
    // are still NoCommittedManifest and must never acquire a second admission.
    let mut restarted = BootstrappedHost::open_for_core_with_test_seams(
        BootstrapOptions {
            config_dir: f.root.clone(),
            ownership_root: f.dir.sockets().to_path_buf(),
            project_root: f.dir.path().canonicalize().unwrap(),
        },
        TestSeams {
            model: restarted_model.clone(),
            sandbox: Arc::new(Direct),
        },
        &evorch_core::recovery_service::CoreGraphRecovery,
    )
    .await
    .unwrap();
    restarted.shutdown().await.unwrap();
    assert_eq!(restarted_model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        restarted_model.admissions_requested.load(Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn partial_recovery_failure_drains_registered_roots_and_reserved_admissions() {
    for registered in [false, true] {
        let f = Fixture::new();
        f.seed();
        f.seed_broken_after_first();
        let model = Arc::new(Model::default());
        model.skip_admission.store(registered, Ordering::SeqCst);
        model.hold_admission.store(true, Ordering::SeqCst);
        let audit = Arc::new(std::sync::Mutex::new(None));
        let capture = audit.clone();
        let result = BootstrappedHost::open_with_service_observer(
            BootstrapOptions {
                config_dir: f.root.clone(),
                ownership_root: f.dir.sockets().to_path_buf(),
                project_root: f.dir.path().canonicalize().unwrap(),
            },
            TestSeams {
                model: model.clone(),
                sandbox: Arc::new(Direct),
            },
            PullRequestAdapters {
                source: Arc::new(FakePullRequestSource::new()),
                logs: Arc::new(Logs::default()),
            },
            move |runtime| *capture.lock().unwrap() = Some(runtime.clone()),
        )
        .await;
        assert!(result.err().unwrap().to_string().contains("restore_failed"));
        assert_eq!(model.admissions_requested.load(Ordering::SeqCst), 1);
        let runtime = audit.lock().unwrap().take().unwrap();
        assert!(runtime.pending_lane_admissions().is_empty());
        assert!(!model.admitting.load(Ordering::SeqCst));
        let runs = runtime.list_agents();
        assert_eq!(runs.len(), usize::from(registered));
        assert!(runs.iter().all(|run| matches!(
            run.phase,
            event_bus::AgentRunPhase::Stopped
                | event_bus::AgentRunPhase::Done
                | event_bus::AgentRunPhase::Error
        )));
        let weak = runtime.downgrade();
        drop(runtime);
        assert!(
            weak.upgrade().is_none(),
            "failed bootstrap retains no runtime task"
        );
    }
}

// Ownership/admission races are logical-runtime contracts, not permission for two
// production cores. Keep them on one real shared-mode writer, without any lease bypass.
struct LogicalHost {
    runtime: runtime::AgentRuntime,
    owner: Arc<runtime::ownership::OwnerHost>,
    scratch_archive: runtime::scratch::archive::ArchiveService,
    services: workbench_host::bootstrap::services::BackgroundServices,
    executor: Arc<tools::ToolExecutor>,
    _service: workbench_host::commands::CommandService,
}
impl LogicalHost {
    async fn shutdown(&mut self) -> Result<(), workbench_host::bootstrap::BootstrapError> {
        workbench_host::bootstrap::lane_pr::stop_and_drain(
            &mut self.services,
            &self.runtime,
            &self.owner,
            &self.executor,
        )
        .await?;
        self.owner.release_ready()?;
        self.scratch_archive.stop_cleanup()?;
        Ok(())
    }
}
impl Fixture {
    fn logical_runtime(&self, model: Arc<Model>, writer: storage::StorageHandle) -> LogicalHost {
        let bus = Arc::new(event_bus::EventBus::new(4096));
        let executor = Arc::new(tools::ToolExecutor::new(bus.clone()));
        let config = StorageConfig {
            db_path: self.root.join("evorch-events.db"),
            ..Default::default()
        };
        let runtime = runtime::AgentRuntime::with_sandbox_factory(
            bus.clone(),
            executor.clone(),
            model,
            Arc::new(Direct),
        )
        .with_run_store(runtime::RunStore::open(&config, writer.clone()).unwrap());
        let owner = Arc::new(
            runtime::ownership::OwnerHost::open(self.dir.sockets(), Default::default(), bus)
                .unwrap(),
        );
        let service = workbench_host::commands::CommandService::new(
            runtime.clone(),
            tokio::runtime::Handle::current(),
        )
        .with_team_writer(writer)
        .with_ownership(owner.clone());
        service.bind_lane_delivery();
        let mut services = workbench_host::bootstrap::services::BackgroundServices::default();
        workbench_host::bootstrap::lane_pr::start(
            &mut services,
            &runtime,
            &owner,
            &owner.thread_scratch(),
            Default::default(),
            PullRequestAdapters {
                source: Arc::new(FakePullRequestSource::new()),
                logs: Arc::new(Logs::default()),
            },
        )
        .unwrap();
        let scratch_archive = runtime::scratch::archive::ArchiveService::new(
            &owner.thread_scratch(),
            owner.scratch_archive_authority(),
        )
        .unwrap();
        LogicalHost {
            runtime,
            owner,
            scratch_archive,
            services,
            executor,
            _service: service,
        }
    }
}
