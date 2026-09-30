use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{AgentRunPhase, Event, EventBus, EventKind};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunStore, RuntimeError,
};
use serde_json::Value;
use storage::{Storage, StorageConfig};
use tools::ToolExecutor;

pub(super) type Call = (&'static str, Value);

struct ScriptedModel(Mutex<VecDeque<ChatResponse>>);

#[async_trait::async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted response"))
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "ledger-ui-test".into()
    }
}

pub(super) async fn execute(
    calls: &[Call],
    config: Option<(&StorageConfig, &Storage)>,
) -> (String, Vec<Event>) {
    let mut responses = VecDeque::new();
    for (index, (name, input)) in calls.iter().enumerate() {
        responses.push_back(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("call-{index}"),
                    name: (*name).into(),
                    input: input.clone(),
                }],
            },
            finish_reason: FinishReason::ToolUse,
            usage: Usage::default(),
        });
    }
    responses.push_back(ChatResponse {
        message: Message {
            role: providers::Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
        },
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    });
    let bus = Arc::new(EventBus::new(256));
    let mut receiver = bus.subscribe();
    let mut runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(ScriptedModel(Mutex::new(responses))),
    );
    if let Some((config, storage)) = config {
        runtime = runtime.with_run_store(RunStore::open(config, storage.handle()).unwrap());
    }
    let run = runtime.delegate_background(Role::Worker, "ledger test".into(), RunConfig::default());
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        loop {
            let event = receiver.recv().await.unwrap();
            let done = matches!(&event.kind, EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Done, .. }
            ) if run_id == &run.to_string());
            events.push(event);
            if done {
                break events;
            }
        }
    })
    .await
    .expect("run completes");
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    (run.to_string(), events)
}
