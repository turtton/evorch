mod support;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, MessageEvent};
use providers::{ChatResponse, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use support::{ScriptedModel, text_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

struct LiveModel {
    buffered: ScriptedModel,
    gate: Option<Arc<Notify>>,
}

#[async_trait]
impl AgentModel for LiveModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Err(RuntimeError::Model {
            reason: "streaming required".into(),
        })
    }

    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: &EventBus,
    ) -> Result<ChatResponse, RuntimeError> {
        for delta in ["first ", "second"] {
            bus.emit(Event::new(MessageEvent::MessageDelta {
                delta: delta.into(),
                run_id: Some(invocation.run_id.clone()),
            }));
        }
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        self.buffered
            .complete(invocation, role, messages, tools)
            .await
    }

    fn selected_model(&self, _: Role) -> String {
        "live".into()
    }
}

fn runtime(model: Arc<dyn AgentModel>, bus: &Arc<EventBus>) -> AgentRuntime {
    AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus.clone())), model)
}

async fn text_until_terminal(receiver: &mut event_bus::EventReceiver) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut text = String::new();
        loop {
            match receiver.recv().await.expect("bus").kind {
                EventKind::Message(MessageEvent::MessageDelta { delta, .. }) => {
                    text.push_str(&delta)
                }
                EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Done | AgentRunPhase::Error,
                    ..
                }) => return text,
                _ => {}
            }
        }
    })
    .await
    .expect("terminal event")
}

#[tokio::test]
async fn deltas_published_before_run_completes() {
    // Given: completion is gated independently of live deltas.
    let gate = Arc::new(Notify::new());
    let model = Arc::new(LiveModel {
        buffered: ScriptedModel::new([Ok(text_response("first second", FinishReason::Stop))]),
        gate: Some(gate.clone()),
    });
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let runtime = runtime(model, &bus);
    // When: start the real agent loop and consume its first delta.
    let run = runtime.delegate_background(Role::Worker, "go".into(), RunConfig::default());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                result = runtime.wait(run) => panic!("completed before gate release: {result:?}"),
                event = receiver.recv() => if let EventKind::Message(MessageEvent::MessageDelta { delta, run_id }) = event.expect("bus").kind {
                    assert_eq!(delta, "first ");
                    assert_eq!(run_id, Some(run.to_string()));
                    break;
                }
            }
        }
    }).await.expect("live delta");
    // Then: no assistant history is committed until completion.
    let before = runtime
        .inspect_agent(run)
        .expect("inspection")
        .message_count;
    gate.notify_one();
    assert_eq!(runtime.wait(run).await.expect("wait"), AgentRunPhase::Done);
    assert_eq!(
        runtime
            .inspect_agent(run)
            .expect("inspection")
            .message_count,
        before + 1
    );
}

#[tokio::test]
async fn final_context_matches_buffered_semantics_no_double_emit() {
    // Given: two turns whose canonical history must match the buffered responses.
    let expected = text_response("first second", FinishReason::ToolUse);
    let model = Arc::new(LiveModel {
        buffered: ScriptedModel::new([
            Ok(expected.clone()),
            Ok(text_response("first second", FinishReason::Stop)),
        ]),
        gate: None,
    });
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let runtime = runtime(model.clone(), &bus);
    // When: complete both turns.
    let run = runtime.delegate_background(Role::Worker, "go".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await.expect("wait"), AgentRunPhase::Done);
    // Then: each canonical response is displayed once and passed intact to the next turn.
    assert_eq!(
        text_until_terminal(&mut receiver).await,
        "first secondfirst second"
    );
    let history = model.buffered.observed().await;
    assert_eq!(history[1].last(), Some(&expected.message));
}

#[tokio::test]
async fn fallback_complete_path_unchanged() {
    // Given: a model implementing only complete.
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "legacy",
        FinishReason::Stop,
    ))]));
    let runtime = runtime(model, &bus);
    // When: execute via the agent loop.
    let run = runtime.delegate_background(Role::Worker, "go".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await.expect("wait"), AgentRunPhase::Done);
    // Then: fallback remains transcript-visible exactly once.
    assert_eq!(text_until_terminal(&mut receiver).await, "legacy");
}

#[tokio::test]
async fn partial_eof_deltas_visible_but_response_not_committed() {
    // Given: a streaming model reporting EOF after publishing partial content.
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let model = Arc::new(LiveModel {
        buffered: ScriptedModel::new([Err(RuntimeError::Model {
            reason: "premature EOF".into(),
        })]),
        gate: None,
    });
    let runtime = runtime(model.clone(), &bus);
    // When: the stream fails through the real loop.
    let run = runtime.delegate_background(Role::Worker, "go".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await.expect("wait"), AgentRunPhase::Error);
    // Then: partial display survives but the committed count remains the input history length.
    assert_eq!(text_until_terminal(&mut receiver).await, "first second");
    assert_eq!(
        runtime
            .inspect_agent(run)
            .expect("inspection")
            .message_count,
        model.buffered.observed().await[0].len()
    );
}
