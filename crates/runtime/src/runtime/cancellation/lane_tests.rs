use super::*;
use event_bus::{LaneBrief, LanePolicy};
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Model {
    calls: AtomicUsize,
    provider_entered: tokio::sync::Notify,
    hold_admission: AtomicBool,
    admission_entered: tokio::sync::Notify,
    release_admission: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl crate::AgentModel for Model {
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &crate::AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        if self.hold_admission.load(Ordering::SeqCst) {
            self.admission_entered.notify_one();
            self.release_admission.notified().await;
        }
        Ok(())
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.provider_entered.notify_one();
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
    run: RunId,
    dir: test_socket_dir::SplitDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let config = storage::StorageConfig {
            db_path: dir.path().join("lanes.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
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
                    "run_id": run, "project_root": null,
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
            run,
            dir,
        }
    }
}

impl Fixture {
    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(self.dir.path().join("lanes.db"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
    async fn start(&self, pending: bool) {
        self.model.hold_admission.store(pending, Ordering::SeqCst);
        assert_eq!(
            self.runtime.recover_unstarted_lanes().unwrap(),
            vec![self.run]
        );
        if pending {
            self.model.admission_entered.notified().await;
        } else {
            self.model.provider_entered.notified().await;
        }
    }
    fn assert_durable_stop(&self) {
        let lane = self.storage.handle().lane("lane").unwrap().unwrap();
        if !lane.is_started() {
            assert_eq!(
                lane.start_blocked_reason.as_deref(),
                Some(storage::lanes::LANE_START_OPERATOR_STOP)
            );
        }
        let destination = self
            .storage
            .handle()
            .lane_delivery_destination("lane-thread".into())
            .unwrap()
            .unwrap();
        assert!(destination.stopped);
        assert_eq!(destination.owner.unwrap().root_run_id, self.run.to_string());
    }
}

#[tokio::test]
async fn pending_lane_stop_is_durable_before_success_and_idempotent_after_settlement() {
    let f = Fixture::new();
    f.start(true).await;
    f.runtime.stop(f.run, StopScope::SelfOnly).unwrap();
    f.assert_durable_stop();
    assert!(f.runtime.wait(f.run).await.is_err());
    f.runtime.stop(f.run, StopScope::SelfOnly).unwrap();
    f.assert_durable_stop();
    let bus = Arc::new(EventBus::new(128));
    let config = storage::StorageConfig {
        db_path: f.dir.path().join("lanes.db"),
        ..Default::default()
    };
    let restarted = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&config, f.storage.handle()).unwrap());
    assert!(restarted.recover_unstarted_lanes().unwrap().is_empty());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_start_stop_write_is_returned_and_an_explicit_retry_commits_both_writes() {
    let f = Fixture::new();
    f.start(true).await;
    f.sql("CREATE TRIGGER fail_lane_stop BEFORE UPDATE ON lanes WHEN NEW.start_blocked_reason IS NOT NULL BEGIN SELECT RAISE(FAIL,'injected lane start Stop failure'); END;");
    assert!(matches!(
        f.runtime.stop(f.run, StopScope::SelfOnly),
        Err(RuntimeError::LaneCancellation { .. })
    ));
    assert!(f.runtime.wait(f.run).await.is_err());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert!(
        f.storage
            .handle()
            .lane("lane")
            .unwrap()
            .unwrap()
            .start_blocked_reason
            .is_none()
    );
    f.sql("DROP TRIGGER fail_lane_stop;");
    f.runtime.stop(f.run, StopScope::SelfOnly).unwrap();
    f.assert_durable_stop();
}

#[tokio::test]
async fn failed_destination_stop_write_never_reports_success_and_retries_after_terminal() {
    for pending in [true, false] {
        let f = Fixture::new();
        f.start(pending).await;
        f.sql("CREATE TRIGGER fail_delivery_stop BEFORE UPDATE ON lane_delivery_destinations BEGIN SELECT RAISE(FAIL,'injected delivery Stop failure'); END;");
        assert!(f.runtime.stop(f.run, StopScope::SelfOnly).is_err());
        let settled = f.runtime.wait(f.run).await;
        if pending {
            assert!(settled.is_err());
        } else {
            assert_eq!(settled.unwrap(), AgentRunPhase::Stopped);
        }
        // A terminal in-memory run is not evidence that its durable Stop committed.
        assert!(f.runtime.stop(f.run, StopScope::SelfOnly).is_err());
        f.sql("DROP TRIGGER fail_delivery_stop;");
        f.runtime.stop(f.run, StopScope::SelfOnly).unwrap();
        f.assert_durable_stop();
    }
}
