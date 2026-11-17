//! run 制御・検査系メタ操作 (wait / cancel / list_agents / inspect_agent)
//! のハンドラ。

use serde::Deserialize;

use super::{DispatchResult, EmptyArgs, error, parse, parse_run_id, serialize, success};
use crate::AgentRuntime;
use crate::agent_loop::LoopState;

#[derive(Deserialize)]
pub(super) struct RunArgs {
    run_id: String,
}

pub(super) fn run_output(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<RunArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let run_id = match parse_run_id(&args.run_id) {
        Ok(run_id) => run_id,
        Err(message) => return error(message),
    };
    match runtime.run_output(state.caller_run_id(), run_id) {
        Ok(output) => {
            let mut dispatch = serialize(&output);
            let terminal =
                serde_json::to_value(&output).is_ok_and(|value| waiting::terminal(&value));
            if terminal || output.completion_notice.is_some() {
                let mut advanced = false;
                if output
                    .completion_notice
                    .as_ref()
                    .is_none_or(|notice| !notice.observed())
                {
                    let progress = super::observations::ChildProgress::new(
                        &run_id.to_string(),
                        output
                            .completion_notice
                            .as_ref()
                            .map(|notice| notice.generation),
                    );
                    advanced = !state.budget.wait_event_observed(&progress.identity());
                    dispatch.observations.child_progress.push(progress);
                }
                dispatch.result.progress = Some(if advanced {
                    tools::Progress::Advanced
                } else {
                    tools::Progress::Stalled
                });
            }
            dispatch
                .observations
                .completions
                .extend(output.completion_notice);
            dispatch
        }
        Err(runtime_error) => {
            let code = match &runtime_error {
                crate::RuntimeError::UnknownRun { .. } => "unknown_run",
                _ => "run_output_denied",
            };
            error(
                serde_json::json!({
                    "code": code,
                    "run_id": args.run_id,
                    "message": runtime_error.to_string(),
                })
                .to_string(),
            )
        }
    }
}

pub(in crate::meta) mod waiting;
pub(crate) use waiting::drop_ignored_arguments;
pub(super) use waiting::wait;

pub(super) fn cancel(runtime: &AgentRuntime, input: serde_json::Value) -> DispatchResult {
    let args = match parse::<RunArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let run_id = match parse_run_id(&args.run_id) {
        Ok(run_id) => run_id,
        Err(message) => return error(message),
    };
    match runtime.cancel(run_id) {
        Ok(()) => success("cancelled"),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}

pub(super) fn list_agents(runtime: &AgentRuntime, input: serde_json::Value) -> DispatchResult {
    if let Err(message) = parse::<EmptyArgs>(input) {
        return error(message);
    }
    serialize(&runtime.list_agents())
}

pub(super) fn inspect_agent(runtime: &AgentRuntime, input: serde_json::Value) -> DispatchResult {
    let args = match parse::<RunArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let run_id = match parse_run_id(&args.run_id) {
        Ok(run_id) => run_id,
        Err(message) => return error(message),
    };
    match runtime.inspect_agent(run_id) {
        Ok(inspection) => serialize(&inspection),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}
