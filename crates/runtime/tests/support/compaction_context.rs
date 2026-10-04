//! Probe the actual prompt and schema overhead, keeping compaction boundary fixtures
//! meaningful when the production tool surface changes.

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{FinishReason, Message};
use runtime::{AgentRuntime, Role, RunConfig};

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
    probe_for_topology(role, false, build).await
}

#[allow(dead_code)] // Only continuation exercises a delegated child.
pub async fn probe_for_child_role(
    role: Role,
    build: impl FnOnce(Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>),
) -> RequestContext {
    probe_for_topology(role, true, build).await
}

async fn probe_for_topology(
    role: Role,
    child: bool,
    build: impl FnOnce(Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>),
) -> RequestContext {
    let model = Arc::new(ScriptedModel::new(
        (0..=usize::from(child)).map(|_| Ok(text_response("probe-done", FinishReason::Stop))),
    ));
    let (runtime, bus) = build(Arc::clone(&model));
    let mut events = bus.subscribe();
    let run_id = if child {
        let parent = runtime.delegate_background(role, "probe-parent".into(), RunConfig::default());
        assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
        runtime
            .delegate_background_as_child(parent, role, "probe", RunConfig::default())
            .expect("probe child is admitted")
    } else {
        runtime.delegate_background(role, "probe".into(), RunConfig::default())
    };
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let tool_tokens = loop {
        if let EventKind::Lifecycle(LifecycleEvent::RunProgress {
            run_id: event_run_id,
            context: Some(context),
            ..
        }) = events
            .recv()
            .await
            .expect("probe event receiver remains open")
            .kind
            && event_run_id == run_id.to_string()
        {
            break context.tool_definitions;
        }
    };
    assert!(
        tool_tokens > 0,
        "standard tool schemas contribute to context"
    );
    RequestContext {
        system: model.wait_for_request(usize::from(child)).await[0].clone(),
        tool_tokens,
    }
}
