//! Continuations only: never call the delegate spawn or generic tool dispatcher.
use super::{DispatchResult, error};
use crate::{
    AgentRuntime,
    agent_loop::LoopState,
    quiesce::{DurableWait, WaitKind},
};

pub(crate) async fn resume_wait(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    wait: &DurableWait,
) -> Vec<(String, tools::ToolResult)> {
    let Some(payload) = &wait.continuation.payload else {
        return vec![(
            wait.tool_call_id.clone(),
            tools::ToolResult::error("missing wait payload"),
        )];
    };
    match &payload.kind {
        WaitKind::Runs => {
            let result = super::runs::waiting::wait(state, runtime, payload.input.clone()).await;
            vec![(wait.tool_call_id.clone(), result.result)]
        }
        WaitKind::Reply { .. } => {
            let result = super::messaging::wait_reply(state, runtime, payload.input.clone()).await;
            vec![(wait.tool_call_id.clone(), result.result)]
        }
        WaitKind::Delegates { calls } => {
            state.delegate_wait_calls = calls.clone();
            let mut children = Vec::with_capacity(calls.len());
            for call in calls {
                children.push(Ok::<_, DispatchResult>(call.child));
            }
            let results = super::delegation::wait_delegates(state, runtime, children).await;
            state.delegate_wait_calls.clear();
            if results.len() != calls.len() {
                return vec![(
                    wait.tool_call_id.clone(),
                    error("incomplete delegate continuation").result,
                )];
            }
            calls
                .iter()
                .zip(results)
                .map(|(call, result)| (call.tool_call_id.clone(), result.result))
                .collect()
        }
    }
}
