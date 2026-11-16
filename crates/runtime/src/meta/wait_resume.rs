//! Continuations only: never call the delegate spawn or generic tool dispatcher.
use super::{DispatchResult, error};
use crate::{
    AgentRuntime,
    agent_loop::LoopState,
    quiesce::{DurableWait, WaitKind},
};

pub(crate) const RETIRED_REPLY_WAIT: &str = "wait_reply has been retired. Use wait for messages and other events. The old tool was not replayed; unread messages remain available.";

pub(crate) async fn resume_wait(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    wait: &DurableWait,
) -> Vec<(String, DispatchResult)> {
    let Some(payload) = &wait.continuation.payload else {
        return vec![(wait.tool_call_id.clone(), error("missing wait payload"))];
    };
    match &payload.kind {
        WaitKind::Runs | WaitKind::Events => {
            let mut input = payload.input.clone();
            super::drop_ignored_arguments("wait", &mut input);
            let result = super::runs::waiting::wait(state, runtime, input).await;
            vec![(wait.tool_call_id.clone(), result)]
        }
        WaitKind::Reply { .. } => {
            vec![(wait.tool_call_id.clone(), error(RETIRED_REPLY_WAIT))]
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
                    error("incomplete delegate continuation"),
                )];
            }
            calls
                .iter()
                .zip(results)
                .map(|(call, result)| (call.tool_call_id.clone(), result))
                .collect()
        }
    }
}
