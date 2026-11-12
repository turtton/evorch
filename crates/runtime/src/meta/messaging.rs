//! AgentRun 間メッセージ系メタ操作 (send / send_message / wait_reply / inbox)
//! のハンドラ。

use std::time::Duration;

use event_bus::{AgentMessageKind, AgentRunPhase};
use serde::Deserialize;

use super::{DispatchResult, EmptyArgs, error, parse, parse_run_id, serialize, success};
use crate::AgentRuntime;
use crate::agent_loop::LoopState;
use crate::runtime::ReplyOutcome;

#[derive(Deserialize)]
pub(super) struct SendMessageArgs {
    run_id: String,
    message: String,
}

#[derive(Deserialize)]
pub(super) struct SendArgs {
    run_id: String,
    message: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct WaitReplyArgs {
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
    // Answers already in the history do not end the wait; a new one does, and
    // flush_user_answers injects its text after this tool result.
    let mut delivered = state.delivered_answer_ids();
    let call_id = state.meta_call_id.clone().unwrap_or_default();
    let saved = state
        .recovery_waits
        .iter()
        .find(|w| w.tool_call_id == call_id);
    if let Some(saved) = saved {
        delivered.extend(saved.answers.iter().map(|g| g.id.clone()));
    }
    let deadline_unix_ms = match saved
        .and_then(|w| w.continuation.payload.as_ref())
        .map(|p| &p.kind)
    {
        Some(crate::quiesce::WaitKind::Reply {
            deadline_unix_ms,
            delivered_answer_ids,
            ..
        }) => {
            delivered.extend(delivered_answer_ids.iter().cloned());
            *deadline_unix_ms
        }
        _ => match crate::agent_loop::tools_waiting::unix_ms().and_then(|now| {
            now.checked_add(args.timeout_ms)
                .ok_or("reply deadline overflow".into())
        }) {
            Ok(deadline) => deadline,
            Err(message) => return error(message),
        },
    };
    let caller = state.caller_run_id();
    let mut cancel = state.channels.cancel_rx.clone();
    let outcome = if let Err(runtime_error) = runtime.check_reply_wait(caller, &args.message_id) {
        // An unknown or foreign message_id fails whatever input is pending.
        WaitOutcome::Finished(Err(runtime_error))
    } else if state.has_pending_user_messages() {
        // Another wait in this same tool batch may already have received input.
        WaitOutcome::UserInput
    } else {
        let mut quiesce = runtime.shared.quiesce.subscribe();
        state
            .quiesce_tools_waiting(Some((&call_id, deadline_unix_ms)))
            .await;
        let remaining_ms = match crate::agent_loop::tools_waiting::unix_ms() {
            Ok(now) => deadline_unix_ms.saturating_sub(now),
            Err(message) => return error(message),
        };
        let wait = runtime.wait_reply_or_answers(
            caller,
            &args.message_id,
            Duration::from_millis(remaining_ms),
            Some(&delivered),
        );
        tokio::pin!(wait);
        loop {
            tokio::select! {
                biased;
                _ = async { let _ = cancel.wait_for(|interrupt| interrupt.is_interrupted()).await; } => {
                    break WaitOutcome::Cancelled;
                }
                result = &mut wait => break WaitOutcome::Finished(result),
                _ = quiesce.changed() => state.quiesce_tools_waiting(Some((&call_id, deadline_unix_ms))).await,
                // Inject after ToolResults, never between ToolUse and ToolResult.
                Some(message) = state.channels.inbox_rx.recv() => {
                    state.queue_user_message(message);
                    if state.has_pending_user_messages() {
                        break WaitOutcome::UserInput;
                    }
                }
            }
        }
    };
    if !matches!(outcome, WaitOutcome::Cancelled) {
        // Input that arrived as the wait ended is kept for the normal injection
        // after this tool batch, as `wait` does.
        while let Ok(message) = state.channels.inbox_rx.try_recv() {
            state.queue_user_message(message);
        }
    }
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return error("parent run could not resume Running");
    }
    match outcome {
        WaitOutcome::Cancelled => error("wait cancelled"),
        WaitOutcome::UserInput => {
            state.budget.mark_progress();
            serialize(&serde_json::json!({
                "status": "user_input_ready",
                "message_id": args.message_id,
            }))
        }
        WaitOutcome::Finished(Ok(ReplyOutcome::Reply(reply))) => serialize(&reply),
        WaitOutcome::Finished(Ok(ReplyOutcome::AnswersReady(question_ids))) => {
            state.budget.mark_progress();
            serialize(&serde_json::json!({
                "status": "answer_ready",
                "question_ids": question_ids,
                "message_id": args.message_id,
            }))
        }
        WaitOutcome::Finished(Err(runtime_error)) => error(runtime_error.to_string()),
    }
}

enum WaitOutcome {
    Cancelled,
    UserInput,
    Finished(Result<ReplyOutcome, crate::RuntimeError>),
}

pub(super) fn inbox(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    if let Err(message) = parse::<EmptyArgs>(input) {
        return error(message);
    }
    match runtime.take_inbox(state.caller_run_id()) {
        Ok(messages) => {
            // Received messages are new input, like a wait that returned them.
            if !messages.is_empty() {
                state.budget.mark_progress();
            }
            serialize(&messages)
        }
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}
