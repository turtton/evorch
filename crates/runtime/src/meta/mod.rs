//! モデルが要求したメタ操作を AgentRuntime API へ接続する。
//!
//! dispatch と共通ヘルパーをここに置き、ハンドラは責務ごとに
//! [`delegation`] / [`messaging`] / [`runs`] へ分離する。

mod delegation;
mod messaging;
mod runs;

use agents::Role;
use serde::Deserialize;
use tools::ToolResult;

use crate::RunId;
use crate::agent_loop::LoopState;

const COMPACT_STUB: &str = "context-engine (v0.2) で提供予定";

pub(crate) struct DispatchResult {
    pub(crate) result: ToolResult,
    pub(crate) finish: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmptyArgs {}

#[derive(Deserialize)]
struct FinishArgs {
    result: String,
}

pub(crate) async fn dispatch(
    state: &mut LoopState,
    name: &str,
    input: serde_json::Value,
) -> DispatchResult {
    let Some(runtime) = state.runtime() else {
        return error("runtime is unavailable");
    };
    match name {
        "delegate_background" => delegation::delegate_background(state, &runtime, input),
        "delegate" => delegation::delegate(state, &runtime, input).await,
        "send" => messaging::send(state, &runtime, input),
        "send_message" => messaging::send_message(state, &runtime, input),
        "wait_reply" => messaging::wait_reply(state, &runtime, input).await,
        "inbox" => messaging::inbox(state, &runtime, input),
        "wait" => runs::wait(state, &runtime, input).await,
        "cancel" => runs::cancel(&runtime, input),
        "list_agents" => runs::list_agents(&runtime, input),
        "inspect_agent" => runs::inspect_agent(&runtime, input),
        "compact" => parse::<EmptyArgs>(input).map_or_else(error, |_| error(COMPACT_STUB)),
        "finish" => finish(input),
        _ => error(format!("unknown meta-op: {name}")),
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

pub(super) fn parse<T: for<'de> Deserialize<'de>>(input: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|parse_error| format!("invalid arguments: {parse_error}"))
}

pub(super) fn parse_role(name: &str) -> Result<Role, String> {
    match name.to_ascii_lowercase().as_str() {
        "orchestrator" => Ok(Role::Orchestrator),
        "explorer" => Ok(Role::Explorer),
        "worker" => Ok(Role::Worker),
        "reviewer" => Ok(Role::Reviewer),
        _ => Err(format!("unknown role: {name}")),
    }
}

/// delegate 系 op が受け付けるタスクカテゴリ (issue #49)。
const CATEGORIES: [&str; 6] = [
    "quick",
    "deep",
    "high-reasoning",
    "visual",
    "writing",
    "research",
];

/// カテゴリ名を 6 種の既知名に検証する。
///
/// 未知の名前は子 run の生成・モデル呼び出しより前にエラーで拒否する
/// (fail-closed)。名前の照合は既知名との完全一致で行う。
pub(super) fn parse_category(name: &str) -> Result<String, String> {
    if CATEGORIES.contains(&name) {
        Ok(name.to_owned())
    } else {
        Err(format!("unknown category: {name}"))
    }
}

pub(super) fn parse_run_id(value: &str) -> Result<RunId, String> {
    let Some(number) = value.strip_prefix("run-") else {
        return Err(format!("invalid run_id: {value}"));
    };
    number
        .parse::<u64>()
        .map(RunId::new)
        .map_err(|_| format!("invalid run_id: {value}"))
}

pub(super) fn serialize(value: &impl serde::Serialize) -> DispatchResult {
    match serde_json::to_string(value) {
        Ok(json) => success(json),
        Err(serialize_error) => error(format!("serialization failed: {serialize_error}")),
    }
}

pub(super) fn success(content: impl Into<String>) -> DispatchResult {
    DispatchResult {
        result: ToolResult::success(content),
        finish: None,
    }
}

pub(super) fn error(content: impl Into<String>) -> DispatchResult {
    DispatchResult {
        result: ToolResult::error(content),
        finish: None,
    }
}
