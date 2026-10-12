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

#[tokio::test]
async fn live_delivery_restores_when_mailbox_closed_before_push() {
    for phase in [AgentRunPhase::Done, AgentRunPhase::Running] {
        // Given: the completed snapshot/mailbox state at the live-to-terminal race seam.
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("race.sqlite3"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(bus, executor, Arc::new(CompletingModel))
            .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let parent =
            runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
        runtime.wait(parent).await.unwrap();
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap();
        runtime.wait(child).await.unwrap();
        lock_runs(&runtime.shared.runs)
            .get(&child)
            .unwrap()
            .phase_tx
            .send_replace(phase);
        // The deterministic seam is delivery after classification, not a scheduler-timed race.
        // When: the live delivery function reaches the now-closed mailbox.
        let result =
            runtime.prepare_delivery(parent, child, AgentMessageKind::Send, "resume".into(), None);
        // Then: exactly one restored trigger is accepted and processed.
        let (_, _, disposition) = result.unwrap();
        assert_eq!(disposition, DeliveryDisposition::Restored);
        assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    }
}

#[tokio::test]
async fn message_driven_restore_rejects_huge_handles_without_consuming_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("handles.sqlite3"),
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
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    runtime.wait(parent).await.unwrap();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    runtime.wait(child).await.unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    let mut record = store.restore_record(child).unwrap().unwrap();
    let mut history: Vec<providers::Message> = serde_json::from_str(&record.messages_json).unwrap();
    history.insert(
        0,
        providers::Message {
            role: providers::Role::User,
            content: vec![providers::ContentBlock::Text {
                text: "saved job-18446744073709551614".into(),
            }],
        },
    );
    record.messages_json = serde_json::to_string(&history).unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    let before = store.restore_record(child).unwrap().unwrap();
    let mut events = runtime.shared.bus.subscribe();
    let result =
        runtime.prepare_delivery(parent, child, AgentMessageKind::Send, "resume".into(), None);
    assert!(matches!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::CorruptContext(_),
            ..
        })
    ));
    assert_eq!(store.restore_record(child).unwrap().unwrap(), before);
    assert_eq!(
        *runtime.entry(child).unwrap().phase_rx.borrow(),
        AgentRunPhase::Done
    );
    assert!(events.drain_pending_snapshot().is_empty());
}
