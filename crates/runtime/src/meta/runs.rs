//! run 制御・検査系メタ操作 (wait / cancel / list_agents / inspect_agent)
//! のハンドラ。

use event_bus::AgentRunPhase;
use serde::Deserialize;

use super::{DispatchResult, EmptyArgs, error, parse, parse_run_id, serialize, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, RunId};

#[derive(Deserialize)]
struct RunArgs {
    run_id: String,
}

pub(super) async fn wait(
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
    wait_for_run(state, runtime, run_id).await
}

async fn wait_for_run(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    run_id: RunId,
) -> DispatchResult {
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        return error("parent run could not enter Waiting");
    }
    let result = runtime.wait(run_id).await;
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return error("parent run could not resume Running");
    }
    match result {
        Ok(phase) => success(format!("{phase:?}")),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}

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
