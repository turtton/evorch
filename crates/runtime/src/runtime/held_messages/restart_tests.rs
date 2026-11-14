use super::*;
use crate::{AgentInvocationContext, AgentModel};

struct DoneModel;
#[async_trait::async_trait]
impl AgentModel for DoneModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
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
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }
}

#[tokio::test]
async fn deferred_wake_rechecks_observation_and_debits_only_once_after_resume() {
    use crate::mailbox::{CompletionNotice, QueuedMessage};
    for observed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("wake.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            Arc::new(DoneModel),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run =
            runtime.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
        runtime.wait(run).await.unwrap();
        let attempt = runtime.begin_restart_quiesce().unwrap();
        let fence = runtime.fence_dispatch_restart().await.unwrap();
        runtime.drain_dispatch_tails(&fence).await.unwrap();
        let notice = CompletionNotice::new(Some(run), RunId::new(99), 1);
        runtime.hold_queued_message(
            run,
            QueuedMessage::completion(
                AgentMessage {
                    message_id: "held-once".into(),
                    sender_run_id: "run-99".into(),
                    recipient_run_id: run.to_string(),
                    kind: AgentMessageKind::Send,
                    content: "completion".into(),
                    reply_to: None,
                },
                notice.clone(),
            ),
        );
        runtime.schedule_wake(run);
        runtime.schedule_wake(run);
        assert!(runtime.shared.auto_wakes.lock().unwrap().is_empty());
        if observed {
            runtime.observe_completion(&notice);
        }
        let mut events = bus.subscribe();
        runtime.cancel_restart_quiesce(&attempt).unwrap();
        if !observed {
            loop {
                let event = events.recv().await.unwrap();
                if matches!(&event.kind, event_bus::EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { run_id }) if run_id == &run.to_string())
                {
                    break;
                }
            }
        }
        runtime.wake(run).await;
        assert_eq!(
            runtime
                .shared
                .auto_wakes
                .lock()
                .unwrap()
                .get(&run)
                .copied()
                .unwrap_or(0),
            u32::from(!observed)
        );
        assert!(
            !events.drain_pending_snapshot().iter().any(|event| matches!(
                &event.kind,
                event_bus::EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { .. })
            ))
        );
    }
}
