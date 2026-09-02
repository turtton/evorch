//! 委譲系メタ操作 (delegate / delegate_background) のハンドラ。

use event_bus::AgentRunPhase;
use serde::Deserialize;

use super::{DispatchResult, error, parse, parse_role, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, RunConfig};

#[derive(Deserialize)]
struct DelegateBackgroundArgs {
    role: String,
    prompt: String,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct DelegateArgs {
    role: String,
    prompt: String,
    #[serde(default)]
    name: Option<String>,
}

pub(super) fn delegate_background(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<DelegateBackgroundArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let role = match parse_role(&args.role) {
        Ok(role) => role,
        Err(message) => return error(message),
    };
    match runtime.delegate_background_as_child(
        state.caller_run_id(),
        role,
        args.prompt,
        RunConfig {
            interactive: args.interactive,
            name: args.name,
        },
    ) {
        Ok(run_id) => success(run_id.to_string()),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}

pub(super) async fn delegate(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<DelegateArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let role = match parse_role(&args.role) {
        Ok(role) => role,
        Err(message) => return error(message),
    };
    let child = match runtime.delegate_background_as_child(
        state.caller_run_id(),
        role,
        args.prompt,
        RunConfig {
            name: args.name,
            ..RunConfig::default()
        },
    ) {
        Ok(child) => child,
        Err(runtime_error) => return error(runtime_error.to_string()),
    };
    state.emit_delegated(&state.caller_run_id().to_string(), &child.to_string());
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        return error("parent run could not enter Waiting");
    }
    let result = runtime.wait(child).await;
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return error("parent run could not resume Running");
    }
    match result {
        Ok(phase) => success(format!("{phase:?}")),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}
