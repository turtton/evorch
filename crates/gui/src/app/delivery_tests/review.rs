//! Receipt reconciliation must survive admission, missing sidebar bindings and lost notifications.
use super::*;
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};

struct AdmissionGate(Arc<tokio::sync::Notify>);
#[async_trait::async_trait]
impl AgentModel for AdmissionGate {
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        self.0.notified().await;
        Ok(())
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

pub(super) fn receipt(status: storage::HostInputStatus) -> storage::HostInputReceipt {
    storage::HostInputReceipt {
        prompt_id: "old".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        text: "retry this".into(),
        image_count: 2,
        status,
        reason: None,
        accepted_at_ns: 1,
        status_at_ns: 1,
    }
}

fn runtime(
    config: &storage::StorageConfig,
    storage: &storage::Storage,
    gate: Arc<tokio::sync::Notify>,
) -> AgentRuntime {
    let bus = Arc::new(event_bus::EventBus::new(128));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(AdmissionGate(gate)),
    )
    .with_run_store(runtime::RunStore::open(config, storage.handle()).unwrap())
}

#[tokio::test]
async fn claimed_admission_keeps_accepted_until_snapshot_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let runtime = runtime(&config, &storage, gate.clone());
    let run = runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "retry this".into(),
            RunConfig {
                initial_prompt_id: Some("old".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(runtime.inspect_agent(run).is_err());
    let saved = state(&dir.path().join("sidebar.json"));
    let mut state = WorkbenchState::new(runtime.clone(), &UiSettings::default())
        .unwrap()
        .with_sidebar(saved.sidebar.clone())
        .with_command_sink(Box::new(crate::runtime_sink::RuntimeCommandSink::new(
            runtime.clone(),
            tokio::runtime::Handle::current(),
        )));
    state.history = saved.history;
    state.history[0].status = DeliveryStatus::Queued;
    state.sidebar.threads[0].run_ids.push(run.to_string());
    state
        .phases
        .insert(run.to_string(), workspace_ui::ThreadRunPhase::Stopped);
    state.reconcile_claimed_thread("thread");
    // Also exercise the sink's generation-checked guard independently of GUI preflight.
    state
        .sink
        .mark_unreachable_host_inputs(&run.to_string())
        .unwrap();
    assert_eq!(
        runtime.host_input_receipts(&["old".into()]).unwrap()[0].status,
        storage::HostInputStatus::Accepted
    );
    assert_eq!(state.history[0].status, DeliveryStatus::Queued);
    gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if runtime.host_input_receipts(&["old".into()]).unwrap()[0].status
                == storage::HostInputStatus::Delivered
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.reconcile_receipts();
    assert_eq!(state.history[0].status, DeliveryStatus::Delivered);
    runtime.stop(run, runtime::StopScope::Subtree).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn restart_recovers_accepted_without_chataccepted_run_binding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar.json");
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .append_host_input_accepted(&[], &[], None, &receipt(storage::HostInputStatus::Accepted))
        .unwrap();
    let mut saved = state(&path);
    saved.history[0].status = DeliveryStatus::Queued;
    assert!(saved.sidebar.threads[0].run_ids.is_empty());
    saved.persist_history().unwrap();
    let runtime = runtime(&config, &storage, Arc::new(tokio::sync::Notify::new()));
    let mut restored = state_for_reload(&path, saved.sidebar.clone()).with_command_sink(Box::new(
        crate::runtime_sink::RuntimeCommandSink::new(runtime, tokio::runtime::Handle::current()),
    ));
    let bus = Arc::new(event_bus::EventBus::new(32));
    let owner =
        runtime::ownership::OwnerHost::open(&dir.path().join("owner"), Default::default(), bus)
            .unwrap();
    let permit = owner.start("thread").unwrap();
    // A different, live owner protects even a run absent from sidebar.run_ids.
    restored
        .restore_history_with_ownership(
            &storage::Database::open(&config).unwrap(),
            &permit.registry_path,
        )
        .unwrap();
    assert_eq!(restored.history[0].status, DeliveryStatus::Queued);
    assert_eq!(
        storage::Database::open(&config)
            .unwrap()
            .host_input_receipts(&["old".into()])
            .unwrap()[0]
            .status,
        storage::HostInputStatus::Accepted
    );
    drop(owner);
    restored
        .restore_history_with_ownership(
            &storage::Database::open(&config).unwrap(),
            &permit.registry_path,
        )
        .unwrap();
    assert_eq!(restored.history[0].status, DeliveryStatus::Unknown);
    assert_eq!(
        storage::Database::open(&config)
            .unwrap()
            .host_input_receipts(&["old".into()])
            .unwrap()[0]
            .status,
        storage::HostInputStatus::Unknown
    );
    assert!(
        restored.composer.queued[0]
            .rejection
            .as_ref()
            .unwrap()
            .contains("duplicate")
    );
    restored.restore_queued_follow_up(0);
    assert_eq!(restored.composer.input, "retry this");
    assert_eq!(restored.composer.images_needed, 2);
    assert_eq!(restored.composer_drafts.len(), 1);
}

#[tokio::test]
async fn claim_recovers_accepted_without_sidebar_run_binding() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .append_host_input_accepted(&[], &[], None, &receipt(storage::HostInputStatus::Accepted))
        .unwrap();
    let runtime = runtime(&config, &storage, Arc::new(tokio::sync::Notify::new()));
    let mut state = state(&dir.path().join("sidebar.json")).with_command_sink(Box::new(
        crate::runtime_sink::RuntimeCommandSink::new(runtime, tokio::runtime::Handle::current()),
    ));
    state.history[0].status = DeliveryStatus::Queued;
    assert!(state.sidebar.threads[0].run_ids.is_empty());
    state.reconcile_claimed_thread("thread");
    assert_eq!(state.history[0].status, DeliveryStatus::Unknown);
    state.restore_queued_follow_up(0);
    assert_eq!(state.composer.input, "retry this");
}

#[test]
fn delivered_to_unknown_with_or_without_notification_retracts_only_matching_bubble() {
    for notify in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(&dir.path().join("sidebar.json"));
        state.history[0].status = DeliveryStatus::Queued;
        let receipts = Arc::new(Mutex::new(vec![receipt(
            storage::HostInputStatus::Delivered,
        )]));
        state.sink = Box::new(Receipts(receipts.clone()));
        state.reconcile_receipts();
        // Identical text is not identity: preserve this unrelated message.
        state
            .transcripts
            .push_host_input("thread", "other", "retry this".into(), Vec::new());
        assert_eq!(
            state
                .transcripts
                .thread()
                .entries()
                .iter()
                .filter(|entry| matches!(
                    entry,
                    crate::model::transcript::TranscriptEntry::UserMessage { .. }
                ))
                .count(),
            2
        );
        receipts.lock().unwrap()[0].status = storage::HostInputStatus::Unknown;
        if notify {
            state.apply_events([event_bus::Event::new(
                event_bus::LifecycleEvent::HostInputUnknown {
                    run_id: "run-1".into(),
                    prompt_id: "old".into(),
                    reason: Some("snapshot_missing".into()),
                },
            )]);
        } else {
            // No notification: repair on the next broad periodic pass.
            state.reconcile_receipts_at(
                std::time::Instant::now() + std::time::Duration::from_secs(3),
            );
        }
        assert_eq!(state.history[0].status, DeliveryStatus::Unknown);
        assert_eq!(
            state
                .transcripts
                .thread()
                .entries()
                .iter()
                .filter(|entry| matches!(
                    entry,
                    crate::model::transcript::TranscriptEntry::UserMessage { .. }
                ))
                .count(),
            1
        );
        assert!(
            state.composer.queued[0]
                .rejection
                .as_ref()
                .unwrap()
                .contains("duplicate")
        );
        state.restore_queued_follow_up(0);
        assert_eq!(state.composer.input, "retry this");
    }
}
