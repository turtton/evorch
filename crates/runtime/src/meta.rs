//! モデルが要求したメタ操作を AgentRuntime API へ接続する。

use agents::Role;
use event_bus::AgentRunPhase;
use serde::Deserialize;
use tools::ToolResult;

use crate::agent_loop::LoopState;
use crate::{AgentRuntime, RunConfig, RunId};

const COMPACT_STUB: &str = "context-engine (v0.2) で提供予定";

pub(crate) struct DispatchResult {
    pub(crate) result: ToolResult,
    pub(crate) finish: Option<String>,
}

#[derive(Deserialize)]
struct DelegateBackgroundArgs {
    role: String,
    prompt: String,
    #[serde(default)]
    interactive: bool,
}

#[derive(Deserialize)]
struct DelegateArgs {
    role: String,
    prompt: String,
}

#[derive(Deserialize)]
struct RunArgs {
    run_id: String,
}

#[derive(Deserialize)]
struct SendMessageArgs {
    run_id: String,
    message: String,
}

#[derive(Deserialize)]
struct FinishArgs {
    result: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

pub(crate) async fn dispatch(
    state: &mut LoopState,
    name: &str,
    input: serde_json::Value,
) -> DispatchResult {
    let Some(runtime) = state.runtime() else {
        return error("runtime is unavailable");
    };
    match name {
        "delegate_background" => delegate_background(&runtime, input),
        "delegate" => delegate(state, &runtime, input).await,
        "send_message" => send_message(state, &runtime, input).await,
        "wait" => wait(state, &runtime, input).await,
        "cancel" => cancel(&runtime, input),
        "list_agents" => list_agents(&runtime, input),
        "inspect_agent" => inspect_agent(&runtime, input),
        "compact" => parse::<EmptyArgs>(input).map_or_else(error, |_| error(COMPACT_STUB)),
        "finish" => finish(input),
        _ => error(format!("unknown meta-op: {name}")),
    }
}

fn delegate_background(runtime: &AgentRuntime, input: serde_json::Value) -> DispatchResult {
    let args = match parse::<DelegateBackgroundArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let role = match parse_role(&args.role) {
        Ok(role) => role,
        Err(message) => return error(message),
    };
    let run_id = runtime.delegate_background(
        role,
        args.prompt,
        RunConfig {
            interactive: args.interactive,
        },
    );
    success(run_id.to_string())
}

async fn delegate(
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
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        return error("parent run could not enter Waiting");
    }
    let result = runtime.delegate(role, args.prompt).await;
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return error("parent run could not resume Running");
    }
    match result {
        Ok(phase) => success(format!("{phase:?}")),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}

async fn send_message(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<SendMessageArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let run_id = match parse_run_id(&args.run_id) {
        Ok(run_id) => run_id,
        Err(message) => return error(message),
    };
    if let Err(runtime_error) = runtime.send_message(run_id, args.message) {
        return error(runtime_error.to_string());
    }
    wait_for_run(state, runtime, run_id).await
}

async fn wait(
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

fn cancel(runtime: &AgentRuntime, input: serde_json::Value) -> DispatchResult {
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

fn list_agents(runtime: &AgentRuntime, input: serde_json::Value) -> DispatchResult {
    if let Err(message) = parse::<EmptyArgs>(input) {
        return error(message);
    }
    serialize(&runtime.list_agents())
}

fn inspect_agent(runtime: &AgentRuntime, input: serde_json::Value) -> DispatchResult {
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

fn finish(input: serde_json::Value) -> DispatchResult {
    match parse::<FinishArgs>(input) {
        Ok(args) => DispatchResult {
            result: ToolResult::success(&args.result),
            finish: Some(args.result),
        },
        Err(message) => error(message),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(input: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|parse_error| format!("invalid arguments: {parse_error}"))
}

fn parse_role(name: &str) -> Result<Role, String> {
    match name.to_ascii_lowercase().as_str() {
        "orchestrator" => Ok(Role::Orchestrator),
        "explorer" => Ok(Role::Explorer),
        "worker" => Ok(Role::Worker),
        "reviewer" => Ok(Role::Reviewer),
        _ => Err(format!("unknown role: {name}")),
    }
}

fn parse_run_id(value: &str) -> Result<RunId, String> {
    let Some(number) = value.strip_prefix("run-") else {
        return Err(format!("invalid run_id: {value}"));
    };
    number
        .parse::<u64>()
        .map(RunId::new)
        .map_err(|_| format!("invalid run_id: {value}"))
}

fn serialize(value: &impl serde::Serialize) -> DispatchResult {
    match serde_json::to_string(value) {
        Ok(json) => success(json),
        Err(serialize_error) => error(format!("serialization failed: {serialize_error}")),
    }
}

fn success(content: impl Into<String>) -> DispatchResult {
    DispatchResult {
        result: ToolResult::success(content),
        finish: None,
    }
}

fn error(content: impl Into<String>) -> DispatchResult {
    DispatchResult {
        result: ToolResult::error(content),
        finish: None,
    }
}
