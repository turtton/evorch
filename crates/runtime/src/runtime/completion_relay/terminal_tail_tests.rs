use super::*;
use crate::runtime::HookPoint;

struct FinishingModel(tokio::sync::Semaphore);

#[async_trait::async_trait]
impl AgentModel for FinishingModel {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.0.acquire().await.unwrap().forget();
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
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "finishing".into()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retirement_and_terminal_relay_overlap_even_with_late_duplicate_stop_or_cancel() {
    for cancel in [None, Some(false), Some(true)] {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("terminal.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let model = Arc::new(FinishingModel(tokio::sync::Semaphore::new(0)));
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime.delegate_background(Role::Worker, "finish".into(), RunConfig::default());
        let execution = runtime.execution_snapshot(run).unwrap();
        let mut pauses = Vec::new();
        for point in [
            HookPoint::TerminalRetiring,
            HookPoint::TerminalIntentRetired,
            HookPoint::TerminalRegistered,
        ] {
            let reached = Arc::new(tokio::sync::Notify::new());
            let notify = reached.clone();
            let (release, held) = std::sync::mpsc::channel();
            runtime.on_execution(point, &execution, move || {
                notify.notify_one();
                held.recv().unwrap();
            });
            pauses.push((reached, release));
        }
        let interrupt = || {
            if let Some(cancel) = cancel {
                if cancel {
                    runtime.cancel(run).unwrap();
                } else {
                    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
                }
            }
        };
        model.0.add_permits(1);
        pauses[0].0.notified().await;
        // Mailbox closure is not completion: the terminal relay is still ahead.
        assert!(!is_live(lock_runs(&runtime.shared.runs).get(&run).unwrap()));
        interrupt();
        interrupt();
        pauses[0].1.send(()).unwrap();
        pauses[1].0.notified().await;
        assert!(
            !runtime
                .resume_intents()
                .unwrap()
                .iter()
                .find(|i| i.run_id == run)
                .unwrap()
                .active
        );
        let attempt = runtime.begin_restart_quiesce().unwrap();
        let fence = runtime.fence_dispatch_restart().await.unwrap();
        {
            let drain = runtime.drain_dispatch_tails(&fence);
            tokio::pin!(drain);
            assert!(futures_util::poll!(&mut drain).is_pending());
        }
        runtime.cancel_restart_quiesce(&attempt).unwrap();
        pauses[1].1.send(()).unwrap();
        pauses[2].0.notified().await;
        // A duplicate after lease transfer must not repopulate the old slot.
        interrupt();
        interrupt();
        let fence = runtime.fence_dispatch_restart().await.unwrap();
        let drain = runtime.drain_dispatch_tails(&fence);
        tokio::pin!(drain);
        assert!(futures_util::poll!(&mut drain).is_pending());
        pauses[2].1.send(()).unwrap();
        runtime.wait(run).await.unwrap();
        drain.await.unwrap();
        assert_eq!(
            runtime.quiesce_state(),
            crate::quiesce::QuiesceState::Checkpointed
        );
    }
}
