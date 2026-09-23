//! Probe the actual prompt and schema overhead, keeping compaction boundary fixtures
//! meaningful when the production tool surface changes.

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{FinishReason, Message};
use runtime::{AgentRuntime, Role, RunConfig};
use tokio::time::{Duration, timeout};

use super::support::{ScriptedModel, text_response};

#[allow(dead_code)] // Individual integration targets use different parts of the shared probe.
pub struct RequestContext {
    pub system: Message,
    pub tool_tokens: u64,
}

impl RequestContext {
    #[allow(dead_code)] // Used by exact-boundary tests only.
    pub fn estimate(&self, messages: &[Message]) -> u64 {
        (serde_json::to_vec(messages)
            .expect("test messages serialize")
            .len() as u64)
            .div_ceil(4)
            + self.tool_tokens
    }
}

#[allow(dead_code)] // Most targets use a Worker; continuation probes Explorer.
pub async fn probe(
    build: impl FnOnce(Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>),
) -> RequestContext {
    probe_for_role(Role::Worker, build).await
}

pub async fn probe_for_role(
    role: Role,
    build: impl FnOnce(Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>),
) -> RequestContext {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "probe-done",
        FinishReason::Stop,
    ))]));
    let (runtime, bus) = build(Arc::clone(&model));
    let mut events = bus.subscribe();
    let run_id = runtime.delegate_background(role, "probe".into(), RunConfig::default());
    assert_eq!(
        timeout(Duration::from_secs(5), runtime.wait(run_id)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    let tool_tokens = timeout(Duration::from_secs(5), async {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::RunProgress {
                context: Some(context),
                ..
            }) = events
                .recv()
                .await
                .expect("probe event receiver remains open")
                .kind
            {
                break context.tool_definitions;
            }
        }
    })
    .await
    .expect("request composition is published");
    assert!(
        tool_tokens > 0,
        "standard tool schemas contribute to context"
    );
    RequestContext {
        system: model.observed().await[0][0].clone(),
        tool_tokens,
    }
}
