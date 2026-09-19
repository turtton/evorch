use super::*;

struct CompletingModel;

#[async_trait::async_trait]
impl AgentModel for CompletingModel {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: providers::Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

struct Fixture {
    runtime: AgentRuntime,
    storage: storage::Storage,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("goal.sqlite3"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(CompletingModel),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        Self {
            runtime,
            storage,
            _dir: dir,
        }
    }

    async fn terminal(&self) -> RunId {
        let run = self.runtime.delegate_background(
            Role::Worker,
            "goal".into(),
            RunConfig {
                name: Some("goal-1".into()),
                network_access: agents::NetworkAccess::Allowed,
                ..RunConfig::default()
            },
        );
        self.runtime.wait(run).await.unwrap();
        run
    }
}

#[tokio::test]
async fn current_authority_replaces_old_authority_when_goal_restored() {
    // Given: an old terminal run with privileged in-memory configuration.
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let run = fixture.terminal().await;
    {
        let mut runs = lock_runs(&runtime.shared.runs);
        let config = &mut runs.get_mut(&run).unwrap().config;
        config.load_skills = vec!["old-skill".into()];
        config.finding_store = Some(fixture._dir.path().into());
        config.delegation_value = Some("old delegation".into());
        config.workspace_branch = Some("old-branch".into());
        config.topology = crate::CoordinationTopology::DynamicTeam { max_workers: 3 };
    }
    // When: the current sender grants only default authority.
    runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    // Then: no old capability is granted to the new execution.
    let entry = runtime.entry(run).unwrap();
    assert_eq!(entry.config.network_access, agents::NetworkAccess::Denied);
    assert!(entry.config.load_skills.is_empty());
    assert!(entry.config.finding_store.is_none());
    assert!(entry.config.delegation_value.is_none());
    assert!(entry.config.workspace_branch.is_none());
    assert_eq!(entry.config.topology, crate::CoordinationTopology::Single);
    assert_eq!(entry.config.name.as_deref(), Some("goal-1"));
}

#[tokio::test]
async fn goal_restore_rejects_disabled_record_or_descriptor() {
    for disable_record in [true, false] {
        // Given: a snapshot whose record or descriptor prohibits restore.
        let fixture = Fixture::new();
        let run = fixture.terminal().await;
        let store = fixture.runtime.shared.run_store.get().unwrap();
        let mut record = store.restore_record(run).unwrap().unwrap();
        let mut descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).unwrap();
        descriptor.non_restorable_reason = Some("test_disabled".into());
        if disable_record {
            record.restorable = false;
        } else {
            descriptor.restorable = false;
        }
        record.config_json = serde_json::to_string(&descriptor).unwrap();
        fixture
            .storage
            .handle()
            .upsert_run_context(&record)
            .unwrap();
        // When: a follow-up tries to restore the same run.
        let result = fixture
            .runtime
            .continue_goal(run, "continue".into(), RunConfig::default());
        // Then: it fails closed and preserves the recorded reason.
        assert!(
            matches!(result, Err(RuntimeError::RunRestoreFailed { reason: RunRestoreFailure::UnsupportedConfig(ref reason), .. }) if reason == "test_disabled")
        );
    }
}

#[tokio::test]
async fn goal_snapshot_consumed_synchronously_before_spawn() {
    // Given: a restorable terminal snapshot on a current-thread executor.
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    // When: restoring without yielding to the spawned task.
    fixture
        .runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    // Then: durable storage already rejects replay of the old snapshot.
    let record = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    assert!(!record.restorable);
    assert!(!descriptor.restorable);
    assert_eq!(
        descriptor.non_restorable_reason.as_deref(),
        Some("snapshot_consumed")
    );
}

#[tokio::test]
async fn goal_restore_emits_restored_event_before_execution() {
    // Given: a terminal goal and a fresh event subscription.
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    let mut events = fixture.runtime.shared.bus.subscribe();
    // When: restoring the goal.
    fixture
        .runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    // Then: observers can distinguish the restore before execution starts.
    assert!(matches!(events.recv().await.unwrap().kind,
        event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunRestored { run_id, message_id, .. })
        if run_id == run.to_string() && !message_id.is_empty()));
}

#[tokio::test]
async fn explicit_current_authority_is_preserved_when_goal_restored() {
    // Given: a terminal goal with default in-memory permissions.
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    lock_runs(&fixture.runtime.shared.runs)
        .get_mut(&run)
        .unwrap()
        .config
        .network_access = agents::NetworkAccess::Denied;
    // When: the current sender explicitly grants network access and a finding store.
    fixture
        .runtime
        .continue_goal(
            run,
            "continue".into(),
            RunConfig {
                network_access: agents::NetworkAccess::Allowed,
                finding_store: Some(fixture._dir.path().into()),
                ..RunConfig::default()
            },
        )
        .unwrap();
    // Then: current grants are retained, rather than replaced with old authority.
    let entry = fixture.runtime.entry(run).unwrap();
    assert_eq!(entry.config.network_access, agents::NetworkAccess::Allowed);
    assert_eq!(
        entry.config.finding_store.as_deref(),
        Some(fixture._dir.path())
    );
}

#[tokio::test]
async fn goal_restore_fails_closed_when_consumed_marker_cannot_be_persisted() {
    // Given: a clean snapshot whose writer has been shut down.
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    drop(fixture.storage);
    // When: a follow-up attempts to consume the snapshot.
    let result = fixture
        .runtime
        .continue_goal(run, "continue".into(), RunConfig::default());
    // Then: no replacement run is registered or restore event emitted.
    assert!(matches!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::SnapshotConsumeFailed(_),
            ..
        })
    ));
    assert_eq!(
        *fixture.runtime.entry(run).unwrap().phase_rx.borrow(),
        AgentRunPhase::Done
    );
}
