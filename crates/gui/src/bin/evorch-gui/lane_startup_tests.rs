use super::*;
use event_bus::{AgentRunPhase, EventReceiver};
use runtime::{AgentInvocationContext, RunId, RunStore, RuntimeError};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
struct AdmissionGate {
    hold: AtomicBool,
    entered: Notify,
    release: Notify,
    completions: AtomicUsize,
}
#[async_trait::async_trait]
impl AgentModel for AdmissionGate {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.completions.fetch_add(1, Ordering::SeqCst);
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: Default::default(),
            usage_reported: true,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "startup-test".into()
    }
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        if self.hold.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}
fn runtime(
    config: &StorageConfig,
    storage: &Storage,
    bus: Arc<EventBus>,
    model: Arc<AdmissionGate>,
) -> AgentRuntime {
    AgentRuntime::new(bus.clone(), Arc::new(tools::ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(config, storage.handle()).unwrap())
}
async fn waiting(events: &mut EventReceiver, id: RunId) {
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id, to, reason, ..
        }) = events.recv().await.unwrap().kind
            && run_id == id.to_string()
        {
            assert_ne!(to, AgentRunPhase::Error, "{reason:?}");
            if to == AgentRunPhase::Waiting {
                return;
            }
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        // Explicitly relative socket paths avoid SUN_LEN without global chdir/env.
        let dir = tempfile::Builder::new()
            .prefix(".lane-startup-")
            .tempdir_in(".")
            .unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("lanes.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let root = PathBuf::from(dir.path().file_name().unwrap()).join("owners");
        Self {
            _dir: dir,
            config,
            storage,
            root,
        }
    }
    fn host(&self, bus: Arc<EventBus>) -> Arc<runtime::ownership::OwnerHost> {
        let host = Arc::new(
            runtime::ownership::OwnerHost::open(&self.root, Default::default(), bus).unwrap(),
        );
        install_host_tombstones(&host, &Default::default());
        host
    }
    fn crashed_lease(&self, lane: &runtime::lanes::LaneOpened, expires_at: u64) {
        // Restore the registry image a killed process leaves, not a cancellation.
        runtime::ownership::Registry::open(&self.root.join("owners.db"))
            .unwrap()
            .update(&lane.thread_id, |owner| {
                owner.state = runtime::ownership::OwnerState::Running;
                owner.lease.expires_at = expires_at;
                Ok(())
            })
            .unwrap();
    }
}
async fn pending_lane(
    f: &Fixture,
) -> (
    AgentRuntime,
    Arc<runtime::ownership::OwnerHost>,
    Arc<AdmissionGate>,
    RunId,
    runtime::lanes::LaneOpened,
) {
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(AdmissionGate::default());
    let first = runtime(&f.config, &f.storage, bus.clone(), model.clone());
    let host = f.host(bus);
    start_owned_runtime(&first, &host).unwrap().await.unwrap();
    let lead = first
        .delegate_chat(
            "lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                ownership: Some(host.start("lead").unwrap()),
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    waiting(&mut events, lead).await;
    model.hold.store(true, Ordering::SeqCst);
    let lane = first
        .open_lane(
            lead,
            "pending-root",
            event_bus::LaneBrief {
                objective: "survive process loss, not cancellation".into(),
                inputs: vec![],
                deliverables: vec!["artifact".into()],
                acceptance_criteria: vec!["review".into()],
                constraints: vec![],
                workspace: Default::default(),
                report_policy: event_bus::LanePolicy::Attached,
            },
        )
        .unwrap();
    model.entered.notified().await;
    assert_eq!(f.storage.handle().unstarted_lanes(10).unwrap().len(), 1);
    assert_eq!(model.completions.load(Ordering::SeqCst), 1); // Lead only.
    (first, host, model, lead, lane)
}
async fn crash_before_root() -> (Fixture, runtime::lanes::LaneOpened) {
    tokio::task::spawn_blocking(|| {
        let old = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let f = Fixture::new();
        let (first, host, _model, _lead, lane) = old.block_on(pending_lane(&f));
        // Destroy the old executor with admission still pending. No cancel API,
        // terminal callback or durable cancellation is executed on either run.
        drop(old);
        drop(first);
        drop(host);
        assert!(
            f.storage
                .handle()
                .lane(&lane.lane_id)
                .unwrap()
                .unwrap()
                .start_blocked_reason
                .is_none()
        );
        (f, lane)
    })
    .await
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn cancelled_pending_intent_is_not_revived_by_a_new_runtime_startup() {
    for subtree in [false, true] {
        let f = Fixture::new();
        let (first, host, model, lead, lane) = pending_lane(&f).await;
        let bus = Arc::new(EventBus::new(256));
        let recovered_model = Arc::new(AdmissionGate::default());
        let next = runtime(&f.config, &f.storage, bus.clone(), recovered_model.clone());
        let current = f.host(bus);
        // The new startup cannot steal the first live host's pending admission.
        let recovery = start_owned_runtime(&next, &current).unwrap();
        assert!(
            next.recover_unstarted_lanes(Some(&current))
                .unwrap()
                .is_empty()
        );
        for _ in 0..2 {
            // Idempotent durable cancellation through both public APIs.
            if subtree {
                first.cancel_subtree(lane.root_run_id).unwrap();
            } else {
                first.cancel(lane.root_run_id).unwrap();
            }
        }
        let cancelled_brief = f
            .storage
            .handle()
            .lane(&lane.lane_id)
            .unwrap()
            .unwrap()
            .brief;
        assert_eq!(
            first
                .open_lane(lead, "pending-root", cancelled_brief)
                .unwrap_err()
                .code,
            "lane_creation_denied"
        );
        model.release.notify_one();
        assert!(matches!(
            first.wait(lane.root_run_id).await,
            Err(RuntimeError::RunTerminated { .. })
        ));
        first.cancel(lead).unwrap();
        first.wait(lead).await.unwrap();
        assert!(!host.quiesce().unwrap());
        drop(first);
        drop(host);
        // Even an expired old lease cannot turn cancellation back into crash recovery.
        f.crashed_lease(&lane, 0);
        recovery.await.unwrap();
        start_owned_runtime(&next, &current).unwrap().await.unwrap();
        assert!(
            next.recover_unstarted_lanes(Some(&current))
                .unwrap()
                .is_empty()
        );
        let record = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
        assert!(record.start_blocked_reason.is_some());
        assert_eq!(record.generation, 0);
        assert!(record.root_run_id.is_none());
        assert_eq!(recovered_model.completions.load(Ordering::SeqCst), 0);
        assert!(!current.quiesce().unwrap());
    }
}

#[tokio::test(start_paused = true)]
async fn startup_recovers_a_crashed_intent_with_a_new_runtime_and_current_host_once() {
    let (f, lane) = crash_before_root().await;
    f.crashed_lease(&lane, u64::MAX);
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(AdmissionGate::default());
    let next = runtime(&f.config, &f.storage, bus.clone(), model.clone());
    let current = f.host(bus);
    let recovery = start_owned_runtime(&next, &current).unwrap();
    assert!(
        next.recover_unstarted_lanes(Some(&current))
            .unwrap()
            .is_empty()
    );
    f.crashed_lease(&lane, 0);
    // No second startup call: virtual time drives the original lease retry driver.
    waiting(&mut events, lane.root_run_id).await;
    recovery.await.unwrap();
    start_owned_runtime(&next, &current).unwrap().await.unwrap();
    assert!(f.storage.handle().unstarted_lanes(10).unwrap().is_empty());
    let record = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    assert!(record.start_blocked_reason.is_none());
    assert_eq!(record.root_run_id, Some(lane.root_run_id.to_string()));
    assert_eq!(record.generation, 1);
    assert_eq!(model.completions.load(Ordering::SeqCst), 1);
    assert_eq!(
        current.attach(&lane.thread_id).unwrap().lease.owner_id,
        current.owner_id()
    );
    current
        .owned_permit(&lane.thread_id)
        .unwrap()
        .validate_generation()
        .unwrap();
    next.cancel(lane.root_run_id).unwrap();
    next.wait(lane.root_run_id).await.unwrap();
    assert!(!current.quiesce().unwrap());
}

#[tokio::test(start_paused = true)]
async fn local_setup_failure_is_held_and_the_startup_driver_finishes() {
    let (f, lane) = crash_before_root().await;
    f.crashed_lease(&lane, 0);
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(AdmissionGate::default());
    let next = runtime(&f.config, &f.storage, bus.clone(), model.clone())
        .with_sandbox_root(f._dir.path().to_path_buf());
    let current = f.host(bus);
    let recovery = start_owned_runtime(&next, &current).unwrap();
    let blocker = f._dir.path().join("not-a-scratch-directory");
    std::fs::write(&blocker, b"file").unwrap();
    // This current-thread test installs the broken scratch before yielding to the
    // admitted root. Sandbox scratch initialization fails before prepared context.
    next.set_thread_scratch(Arc::new(runtime::scratch::ThreadScratch::at(blocker)));
    assert_eq!(
        next.wait(lane.root_run_id).await.unwrap(),
        AgentRunPhase::Error
    );
    recovery.await.unwrap(); // Completes from a held terminal, not a fixed deadline.
    let record = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    assert!(record.start_blocked_reason.is_some());
    assert_eq!(record.generation, 0);
    assert_eq!(model.completions.load(Ordering::SeqCst), 0);
    assert!(f.storage.handle().unstarted_lanes(10).unwrap().is_empty());
    start_owned_runtime(&next, &current).unwrap().await.unwrap();
    assert!(!current.quiesce().unwrap());
}
