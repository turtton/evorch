use super::*;
use crate::commands::{ContinueInput, HostCommand};
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, RunStore, RuntimeError};
use std::sync::Arc;

struct IdleModel;
#[async_trait::async_trait]
impl AgentModel for IdleModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "idle".into(),
                }],
            },
            usage: Default::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "idle-test".into()
    }
}
async fn waiting(events: &mut EventReceiver, run: RunId) {
    loop {
        if matches!(events.recv().await.unwrap().kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == run.to_string())
        {
            return;
        }
    }
}
fn continuation(automatic: bool) -> HostCommand {
    HostCommand::ContinueChat(ContinueInput {
        project_root: None,
        thread_id: "stop-pending".into(),
        composer_role: ComposerRole::Orchestrator,
        model_preference: None,
        automatic,
    })
}
#[tokio::test]
async fn stop_pending_continue_reaches_runtime_but_ordinary_live_continue_is_not_applied() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(IdleModel),
    )
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime
        .delegate_chat(
            "stop-pending",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current());
    service
        .bindings
        .chat_runs
        .insert("stop-pending".into(), run);
    waiting(&mut events, run).await;
    assert!(!runtime.graceful_stop_requested(run));
    assert!(matches!(
        service
            .submit_authorized(continuation(false), None)
            .as_slice(),
        [HostEvent::ChatNotice {
            outcome: NoticeOutcome::NotApplied,
            ..
        }]
    ));
    service.submit_authorized(
        HostCommand::StopChat {
            thread_id: "stop-pending".into(),
        },
        None,
    );
    // No await: Stop is recorded, but this executor cannot retire the old run yet.
    assert_eq!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Waiting
    );
    assert!(runtime.graceful_stop_requested(run));
    assert!(matches!(
        service
            .submit_authorized(continuation(true), None)
            .as_slice(),
        [HostEvent::ChatNotice {
            outcome: NoticeOutcome::NotApplied,
            ..
        }]
    ));
    let accepted = service.submit_authorized(continuation(false), None);
    assert!(
        matches!(accepted.as_slice(), [HostEvent::ChatAccepted { run_id, .. }] if run_id == &run.to_string()),
        "graceful Stop-pending Continue must reach runtime: {accepted:?}"
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);
    waiting(&mut events, run).await;
    assert!(!runtime.graceful_stop_requested(run));
    assert!(matches!(
        service
            .submit_authorized(continuation(false), None)
            .as_slice(),
        [HostEvent::ChatNotice {
            outcome: NoticeOutcome::NotApplied,
            ..
        }]
    ));
    runtime.stop(run, runtime::StopScope::Subtree).unwrap();
    runtime.wait(run).await.unwrap();
}
