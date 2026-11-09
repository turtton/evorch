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
use storage::{Storage, StorageConfig, StorageHandle};
use tokio::sync::Notify;
use workbench_host::bootstrap::{
    BootstrapOptions, BootstrappedHost, TestSeams, lane_pr::PullRequestAdapters,
};

#[derive(Default)]
struct Model {
    entered: Notify,
    calls: AtomicUsize,
    hold_admission: AtomicBool,
    admission: Notify,
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
        true
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        if self.hold_admission.load(Ordering::SeqCst) {
            self.admission.notify_one();
            std::future::pending().await
        } else {
            Ok(())
        }
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
    dir: tempfile::TempDir,
    storage: Storage,
    root: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        // Kept relative: both executors bind an owner socket below this root.
        let dir = tempfile::Builder::new()
            .prefix(".bg-")
            .tempdir_in(".")
            .unwrap();
        let root = std::path::PathBuf::from(dir.path().file_name().unwrap());
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
        Self { dir, storage, root }
    }
    fn seed(&self) {
        self.seed_with_ownership(true);
    }
    fn seed_with_ownership(&self, requires_ownership: bool) {
        self.storage.handle().create_lane(storage::lanes::NewLane {
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
                ownership_root: self.root.join("owners"),
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
    fn handle(&self) -> StorageHandle {
        self.storage.handle()
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
    let policy = f.handle().pr_auto_fix_state(key()).unwrap();
    service
        .set_auto_fix("lane-thread", 1, policy.policy_revision, true, &permit)
        .await
        .unwrap();
    gate.release_ok(fetched(&head));
    logs.entered.notified().await;
    let notices = f.handle().pr_notices(key(), String::new()).unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].mode,
        storage::pull_request_auto_fix::PrNoticeMode::Auto
    );
    assert_eq!(source.calls().len(), 1);
    assert!(logs.active.load(Ordering::SeqCst));
    core.shutdown().await.unwrap();
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
async fn embedded_and_core_share_thread_authority_without_duplicate_recovery_or_monitor() {
    let f = Fixture::new();
    let head = checkout(f.dir.path());
    f.seed();
    f.handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "core-marker".into(),
            ..Default::default()
        }])
        .unwrap();
    let model = Arc::new(Model::default());
    let embedded_source = Arc::new(FakePullRequestSource::new());
    let gate = embedded_source.push_gate(&key());
    let logs = Arc::new(Logs::default());
    // Both adapters use exactly the shared factory used by the embedded GUI.
    // No renderer or GUI dependency is needed to exercise the authority boundary.
    let mut embedded = f
        .core(model.clone(), embedded_source.clone(), logs.clone())
        .await;
    model.entered.notified().await;
    let other_model = Arc::new(Model::default());
    let other_source = Arc::new(FakePullRequestSource::new());
    let mut core = f
        .core(other_model.clone(), other_source.clone(), logs.clone())
        .await;
    assert!(core.owner.owned_permit("lane-thread").is_err());
    assert!(core.runtime.inspect_agent(RunId::new(42)).is_err());
    let permit = embedded.owner.owned_permit("lane-thread").unwrap();
    embedded
        .runtime
        .pull_requests()
        .unwrap()
        .link("lane-thread", "owner/repo#7", 0, &permit)
        .await
        .unwrap();
    gate.arrived().await;
    let service = embedded.runtime.pull_requests().unwrap();
    let mut events = embedded.runtime.event_bus().subscribe();
    published(&core.runtime.event_bus());
    published(&embedded.runtime.event_bus());
    candidate(&mut events).await;
    let policy = f.handle().pr_auto_fix_state(key()).unwrap();
    service
        .set_auto_fix("lane-thread", 1, policy.policy_revision, true, &permit)
        .await
        .unwrap();
    gate.release_ok(fetched(&head));
    logs.entered.notified().await;
    // An owned marker fetch proves the other scheduler has scanned the same DB
    // after the embedded link existed. It must not also launch key 7.
    let marker = PullRequestKey::github("owner/repo", 8).unwrap();
    let marker_gate = other_source.push_gate(&marker);
    let marker_permit = core.owner.start("core-marker").unwrap();
    core.runtime
        .pull_requests()
        .unwrap()
        .link("core-marker", "owner/repo#8", 0, &marker_permit)
        .await
        .unwrap();
    marker_gate.arrived().await;
    // Draining is a terminal barrier: assertions do not infer quiescence from time.
    core.shutdown().await.unwrap();
    assert_eq!(
        other_source
            .calls()
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>(),
        [marker]
    );
    drop(marker_gate);
    assert_eq!(other_model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(embedded_source.calls().len(), 1);
    assert_eq!(logs.calls.load(Ordering::SeqCst), 1);
    assert!(logs.active.load(Ordering::SeqCst)); // stopping core must not stop the other owner
    let notices = f.handle().pr_notices(key(), String::new()).unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].mode,
        storage::pull_request_auto_fix::PrNoticeMode::Auto
    );
    embedded.shutdown().await.unwrap();
    assert!(!logs.active.load(Ordering::SeqCst));
}

#[tokio::test]
async fn concurrent_startup_does_not_steal_a_live_pending_lane_and_stops_retrying_on_drain() {
    for requires_ownership in [true, false] {
        let f = Fixture::new();
        f.seed_with_ownership(requires_ownership);
        let model = Arc::new(Model::default());
        model.hold_admission.store(true, Ordering::SeqCst);
        let mut embedded = f
            .core(
                model.clone(),
                Arc::new(FakePullRequestSource::new()),
                Arc::new(Logs::default()),
            )
            .await;
        model.admission.notified().await;
        assert_eq!(f.handle().unstarted_lanes(10).unwrap().len(), 1);
        let other = Arc::new(Model::default());
        let mut core = f
            .core(
                other.clone(),
                Arc::new(FakePullRequestSource::new()),
                Arc::new(Logs::default()),
            )
            .await;
        assert!(
            core.runtime
                .recover_unstarted_lanes(Some(&core.owner))
                .unwrap()
                .is_empty()
        );
        core.shutdown().await.unwrap();
        assert_eq!(other.calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.handle().unstarted_lanes(10).unwrap().len(), 1);
        embedded
            .runtime
            .stop(RunId::new(42), runtime::StopScope::SelfOnly)
            .unwrap();
        embedded.shutdown().await.unwrap();
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    }
}
