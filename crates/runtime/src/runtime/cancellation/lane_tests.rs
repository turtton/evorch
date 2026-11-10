use super::{tests::pause, *};
use crate::ownership::OwnerHost;
use event_bus::{LaneBrief, LanePolicy};
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Model(AtomicUsize);

#[async_trait::async_trait]
impl crate::AgentModel for Model {
    fn requires_admission(&self) -> bool {
        true
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

struct Fixture {
    runtime: AgentRuntime,
    model: Arc<Model>,
    storage: storage::Storage,
    owner: OwnerHost,
    run: RunId,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("lanes.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let owner =
            OwnerHost::open(&dir.path().join("owners"), Default::default(), bus.clone()).unwrap();
        let model = Arc::new(Model::default());
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime.reserve_run_id();
        storage
            .handle()
            .create_lane(storage::lanes::NewLane {
                lane_id: "lane".into(),
                thread_id: "lane-thread".into(),
                origin_thread_id: "lead-thread".into(),
                origin_run_id: "lead".into(),
                creation_key: "create".into(),
                brief: LaneBrief {
                    objective: "a fenced lane".into(),
                    inputs: vec![],
                    deliverables: vec!["artifact".into()],
                    acceptance_criteria: vec!["review".into()],
                    constraints: vec![],
                    verification_requirements: Default::default(),
                    workspace: Default::default(),
                    report_policy: LanePolicy::Attached,
                },
                start_seed: serde_json::json!({
                    "run_id": run, "project_root": null, "requires_ownership": true,
                })
                .to_string(),
                project_id: None,
                project_root: None,
                title: None,
                at_ns: 1,
            })
            .unwrap();
        Self {
            runtime,
            model,
            storage,
            owner,
            run,
            dir,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_after_lane_preflight_cannot_write_initial_context_or_failure_report() {
    let f = Fixture::new();
    let permit = f.owner.start("lane-thread").unwrap();
    let execution = f
        .runtime
        .reserve_admission(f.run, None, Some(permit.clone()));
    let (reached, release) = pause(&f.runtime, HookPoint::LaneOwnershipValidated, &execution);
    assert_eq!(
        f.runtime.recover_unstarted_lanes(Some(&f.owner)).unwrap(),
        vec![f.run]
    );
    reached.recv().unwrap();
    let next = OwnerHost::open(
        &f.dir.path().join("owners"),
        Default::default(),
        Arc::new(EventBus::new(128)),
    )
    .unwrap();
    f.owner.handoff(&permit, &next).unwrap();
    release.send(()).unwrap();
    assert_eq!(f.runtime.wait(f.run).await.unwrap(), AgentRunPhase::Error);
    // Registration already succeeded; do not retroactively change that result.
    assert!(matches!(
        f.runtime.admission_snapshot(f.run).unwrap().result,
        Some(Ok(()))
    ));
    assert_eq!(
        lock_runs(&f.runtime.shared.runs)[&f.run]
            .terminal_reason
            .as_deref(),
        Some(
            RuntimeError::StaleOwnership {
                run_id: f.run.to_string()
            }
            .to_string()
            .as_str()
        )
    );
    assert_eq!(f.model.0.load(Ordering::SeqCst), 0);
    assert!(
        f.runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(f.run)
            .unwrap()
            .is_none()
    );
    let lane = f.storage.handle().lane("lane").unwrap().unwrap();
    assert!(!lane.is_started());
    assert_eq!(lane.latest_report_seq, 0);
    assert!(
        f.storage
            .handle()
            .lane_reports("lane", 0, 10)
            .unwrap()
            .is_empty()
    );
}
