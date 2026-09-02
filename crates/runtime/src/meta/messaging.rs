//! AgentRun 間メッセージ系メタ操作 (send / send_message / wait_reply / inbox)
//! のハンドラ。

use std::time::Duration;

use event_bus::{AgentMessageKind, AgentRunPhase};
use serde::Deserialize;

use super::{DispatchResult, EmptyArgs, error, parse, parse_run_id, serialize, success};
use crate::AgentRuntime;
use crate::agent_loop::LoopState;

#[derive(Deserialize)]
struct SendMessageArgs {
    run_id: String,
    message: String,
}

#[derive(Deserialize)]
struct SendArgs {
    run_id: String,
    message: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
}

#[derive(Deserialize)]
struct WaitReplyArgs {
    message_id: String,
    timeout_ms: u64,
}

/// send / send_message 共通の配送指示。
struct SendInstruction {
    run_id: String,
    message: String,
    kind: AgentMessageKind,
    reply_to: Option<String>,
}

pub(super) fn send(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<SendArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let kind = match parse_message_kind(args.kind.as_deref()) {
        Ok(kind) => kind,
        Err(message) => return error(message),
    };
    if kind == AgentMessageKind::Reply && args.reply_to.is_none() {
        return error("Reply には reply_to が必要です");
    }
    deliver(
        state,
        runtime,
        SendInstruction {
            run_id: args.run_id,
            message: args.message,
            kind,
            reply_to: args.reply_to,
        },
    )
}

/// `send_message` は `send` の fire-and-forget alias であり、
/// 旧引数形 `{run_id, message}` のまま強制 `kind=send` で配送する。
pub(super) fn send_message(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<SendMessageArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    deliver(
        state,
        runtime,
        SendInstruction {
            run_id: args.run_id,
            message: args.message,
            kind: AgentMessageKind::Send,
            reply_to: None,
        },
    )
}

fn deliver(
    state: &LoopState,
    runtime: &AgentRuntime,
    instruction: SendInstruction,
) -> DispatchResult {
    let recipient = match parse_run_id(&instruction.run_id) {
        Ok(recipient) => recipient,
        Err(message) => return error(message),
    };
    match runtime.send_agent_message(
        state.caller_run_id(),
        recipient,
        instruction.kind,
        instruction.message,
        instruction.reply_to,
    ) {
        Ok(message_id) => success(message_id),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}

fn parse_message_kind(value: Option<&str>) -> Result<AgentMessageKind, String> {
    match value {
        None => Ok(AgentMessageKind::Send),
        Some("send") => Ok(AgentMessageKind::Send),
        Some("reply") => Ok(AgentMessageKind::Reply),
        Some("steering") => Ok(AgentMessageKind::Steering),
        Some(other) => Err(format!("unknown kind: {other}")),
    }
}

pub(super) async fn wait_reply(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<WaitReplyArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        return error("parent run could not enter Waiting");
    }
    let result = runtime
        .wait_reply(
            state.caller_run_id(),
            &args.message_id,
            Duration::from_millis(args.timeout_ms),
        )
        .await;
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return error("parent run could not resume Running");
    }
    match result {
        Ok(reply) => serialize(&reply),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}

pub(super) fn inbox(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    if let Err(message) = parse::<EmptyArgs>(input) {
        return error(message);
    }
    match runtime.take_inbox(state.caller_run_id()) {
        Ok(messages) => serialize(&messages),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}
