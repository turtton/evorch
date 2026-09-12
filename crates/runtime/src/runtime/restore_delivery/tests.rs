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

    fn selected_model(&self, _: Role) -> String {
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
