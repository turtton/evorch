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

#[tokio::test(start_paused = true)]
async fn startup_recovers_a_durable_intent_with_a_new_runtime_and_current_host_once() {
    // A relative private root keeps Unix socket addresses below SUN_LEN even
    // under a long checkout/TMPDIR, without chdir or process-global env changes.
    let dir = tempfile::Builder::new()
        .prefix(".lane-startup-")
        .tempdir_in(".")
        .unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("lanes.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(AdmissionGate::default());
    let first = runtime(&config, &storage, bus.clone(), model.clone());
    let root = PathBuf::from(dir.path().file_name().unwrap()).join("owners");
    let host =
        Arc::new(runtime::ownership::OwnerHost::open(&root, Default::default(), bus).unwrap());
    install_host_tombstones(&host, &Default::default());
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
            "crash-before-root",
            event_bus::LaneBrief {
                objective: "survive restart".into(),
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
    assert_eq!(storage.handle().unstarted_lanes(10).unwrap().len(), 1);

    let bus = Arc::new(EventBus::new(256));
    let mut recovered_events = bus.subscribe();
    let recovered_model = Arc::new(AdmissionGate::default());
    let next = runtime(&config, &storage, bus.clone(), recovered_model.clone());
    let current =
        Arc::new(runtime::ownership::OwnerHost::open(&root, Default::default(), bus).unwrap());
    install_host_tombstones(&current, &Default::default());
    // A second window cannot recover work the first host still owns.
    let recovery = start_owned_runtime(&next, &current).unwrap();
    assert!(
        next.recover_unstarted_lanes(Some(&current))
            .unwrap()
            .is_empty()
    );

    // Emulate process loss at the persisted-intent / admission boundary: stop
    // every old task, leaving no prepared root and the durable intent untouched.
    first.cancel(lane.root_run_id).unwrap();
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

    // Restore the registry image of a crashed (not gracefully released) host.
    let mut registry = runtime::ownership::Registry::open(&root.join("owners.db")).unwrap();
    registry
        .update(&lane.thread_id, |owner| {
            owner.state = runtime::ownership::OwnerState::Running;
            owner.lease.expires_at = u64::MAX;
            Ok(())
        })
        .unwrap();
    assert!(
        next.recover_unstarted_lanes(Some(&current))
            .unwrap()
            .is_empty()
    );
    registry
        .update(&lane.thread_id, |owner| {
            owner.lease.expires_at = 0;
            Ok(())
        })
        .unwrap();
    // No second startup call: its retry driver claims the expired generation.
    // Tokio virtual time drives lease polling, never wall-clock test deadlines.
    waiting(&mut recovered_events, lane.root_run_id).await;
    recovery.await.unwrap();
    start_owned_runtime(&next, &current).unwrap().await.unwrap();
    assert!(storage.handle().unstarted_lanes(10).unwrap().is_empty());
    let record = storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    assert_eq!(record.root_run_id, Some(lane.root_run_id.to_string()));
    assert_eq!(record.generation, 1);
    assert_eq!(recovered_model.completions.load(Ordering::SeqCst), 1);
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
