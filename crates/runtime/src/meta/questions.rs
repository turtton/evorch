use super::{DispatchResult, EmptyArgs, error, parse, serialize};
use crate::{AgentRuntime, agent_loop::LoopState};
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AskArgs {
    title: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default = "blocking_default")]
    blocking: bool,
}
fn blocking_default() -> bool {
    true
}
pub(super) fn ask_user(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<AskArgs>(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    match runtime.request_user_question(
        state.caller_run_id(),
        args.title,
        args.options,
        args.blocking,
    ) {
        Ok(q) => serialize(
            &serde_json::json!({"question_id":q.id,"status":"pending","blocking":q.blocking,"delivery":"Answer arrives at a turn boundary. Continue independent work; do not poll."}),
        ),
        Err(e) => error(e),
    }
}
pub(super) fn user_answers(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    if let Err(e) = parse::<EmptyArgs>(input) {
        return error(e);
    }
    match runtime.user_answers(state.caller_run_id()) {
        Ok(q) => serialize(&q),
        Err(e) => error(e),
    }
}
