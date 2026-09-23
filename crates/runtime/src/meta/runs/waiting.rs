//! Bounded, event-driven observation of directly related runs. Waiting never
//! consumes mailboxes, restores runs, or cancels the observed work.

use std::collections::HashSet;
use std::time::Duration;

use event_bus::AgentRunPhase;
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::super::{DispatchResult, error, parse, parse_run_id, serialize, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, RunId};

pub(in crate::meta) const MAX_WAIT_RUNS: usize = 8;
pub(in crate::meta) const MAX_WAIT_MS: u64 = 600_000;
const OUTPUT_BYTES: usize = 4096;
const REASON_BYTES: usize = 1024;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WaitMode {
    #[default]
    Any,
    All,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::meta) struct WaitArgs {
    run_id: Option<String>,
    run_ids: Option<Vec<String>>,
    mode: Option<WaitMode>,
    timeout_ms: Option<u64>,
}

struct WaitRequest {
    runs: Vec<RunId>,
    mode: WaitMode,
    timeout: Duration,
    legacy: bool,
}

impl WaitArgs {
    fn validate(self) -> Result<WaitRequest, String> {
        let legacy = self.run_id.is_some()
            && self.run_ids.is_none()
            && self.mode.is_none()
            && self.timeout_ms.is_none();
        let runs = match (self.run_id, self.run_ids) {
            (Some(run), None) => vec![run],
            (None, Some(runs)) if (1..=MAX_WAIT_RUNS).contains(&runs.len()) => runs,
            (None, Some(_)) => return Err("run_ids must contain 1 to 8 runs".into()),
            _ => return Err("provide exactly one of run_id or run_ids".into()),
        };
        let timeout_ms = self.timeout_ms.unwrap_or(MAX_WAIT_MS);
        if timeout_ms > MAX_WAIT_MS {
            return Err(format!("timeout_ms must be between 0 and {MAX_WAIT_MS}"));
        }
        let runs = runs
            .iter()
            .map(|id| parse_run_id(id))
            .collect::<Result<Vec<_>, _>>()?;
        if runs.iter().copied().collect::<HashSet<_>>().len() != runs.len() {
            return Err("run_ids must not contain duplicates".into());
        }
        Ok(WaitRequest {
            runs,
            mode: self.mode.unwrap_or_default(),
            timeout: Duration::from_millis(timeout_ms),
            legacy,
        })
    }
}

pub(in crate::meta) async fn wait(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> DispatchResult {
    let request = match parse::<WaitArgs>(input).and_then(WaitArgs::validate) {
        Ok(request) => request,
        Err(message) => return error(message),
    };
    // Validate every target before suspending the parent or observing any output.
    if let Err(message) = snapshots(runtime, state.caller_run_id(), &request.runs) {
        return error(message);
    }
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        return error("parent run could not enter Waiting");
    }
    let caller = state.caller_run_id();
    let mut result = if *state.channels.cancel_rx.borrow() {
        Err("wait cancelled".into())
    } else if state.has_pending_user_messages() {
        // Another wait in this same tool batch may already have received input.
        user_input_response(runtime, caller, &request)
    } else {
        tokio::select! {
            biased;
            // Observe cancellation/terminal state first. If observation wins a
            // race with UI delivery, the receiver retains the input for the next
            // tool round boundary.
            result = observe(runtime, caller, &request, state.channels.cancel_rx.clone()) => result,
            Some(message) = state.channels.inbox_rx.recv() => {
                // Inject after ToolResults, never between ToolUse and ToolResult.
                state.queue_user_message(message);
                user_input_response(runtime, caller, &request)
            }
        }
    };
    if let Ok(result) = &mut result {
        // If terminal observation won the select, preserve any concurrently
        // queued UI input for delivery after this tool batch as well.
        while let Ok(message) = state.channels.inbox_rx.try_recv() {
            state.queue_user_message(message);
        }
        if state.has_pending_user_messages() {
            result["user_input_ready"] = Value::Bool(true);
            result["timed_out"] = Value::Bool(false);
        }
    }
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return error("parent run could not resume Running");
    }
    match result {
        Ok(result)
            if request.legacy
                && result["timed_out"] == false
                && result["inbox_ready"] == false
                && result["user_input_ready"] == false
                && result["attention_run_ids"]
                    .as_array()
                    .is_some_and(Vec::is_empty) =>
        {
            // Preserve the established single-run success result.
            success(result["runs"][0]["phase"].as_str().unwrap_or("Error"))
        }
        Ok(result) => serialize(&result),
        Err(message) => error(message),
    }
}

fn user_input_response(
    runtime: &AgentRuntime,
    caller: RunId,
    request: &WaitRequest,
) -> Result<Value, String> {
    let runs = snapshots(runtime, caller, &request.runs)?;
    let mailbox = runtime
        .run_mailbox(caller)
        .map_err(|error| error.to_string())?;
    let mut result = response(runs, request.mode, mailbox.has_interrupting_message());
    result["timed_out"] = Value::Bool(false);
    result["user_input_ready"] = Value::Bool(true);
    Ok(result)
}

async fn observe(
    runtime: &AgentRuntime,
    caller: RunId,
    request: &WaitRequest,
    mut cancel: watch::Receiver<bool>,
) -> Result<Value, String> {
    if *cancel.borrow() {
        return Err("wait cancelled".into());
    }
    let mut questions = runtime.shared.question_version.subscribe();
    let initial = snapshots(runtime, caller, &request.runs)?;
    let mailbox = runtime
        .run_mailbox(caller)
        .map_err(|error| error.to_string())?;
    // Subscribe before checking unread messages so delivery cannot race the
    // initial snapshot. Notifications alone are insufficient: drains and closure
    // also change the mailbox version, and completion relays must respect mode.
    let mut inbox_changes = mailbox.subscribe_version();
    let inbox_ready = mailbox.has_interrupting_message();
    if satisfied(&initial, request.mode)
        || needs_attention(&initial)
        || inbox_ready
        || request.timeout.is_zero()
    {
        return Ok(response(initial, request.mode, inbox_ready));
    }
    // AgentRuntime::wait subscribes to each run's existing watch channel. A
    // completion between this snapshot and subscription is read immediately.
    let mut pending = request
        .runs
        .iter()
        .map(|run| runtime.wait(*run))
        .collect::<FuturesUnordered<_>>();
    let deadline = tokio::time::sleep(request.timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return Err("wait cancelled".into());
                }
            }
            _ = &mut deadline => {
                return snapshots(runtime, caller, &request.runs)
                    .map(|runs| response(runs, request.mode, mailbox.has_interrupting_message()));
            }
            changed = inbox_changes.changed() => {
                if changed.is_ok() && mailbox.has_interrupting_message() {
                    return snapshots(runtime, caller, &request.runs)
                        .map(|runs| response(runs, request.mode, true));
                }
            }
            changed = questions.changed() => {
                if changed.is_ok() {
                    let runs = snapshots(runtime, caller, &request.runs)?;
                    if needs_attention(&runs) {
                        return Ok(response(runs, request.mode, mailbox.has_interrupting_message()));
                    }
                }
            }
            completed = pending.next() => {
                // Admission failures also become terminal snapshots; they are
                // outcomes of the target run rather than failures of this wait.
                let _ = completed;
                let runs = snapshots(runtime, caller, &request.runs)?;
                if satisfied(&runs, request.mode) || needs_attention(&runs) || pending.is_empty() {
                    return Ok(response(runs, request.mode, mailbox.has_interrupting_message()));
                }
            }
        }
    }
}

fn terminal(run: &Value) -> bool {
    matches!(
        run["status"].as_str(),
        Some("completed" | "cancelled" | "failed")
    )
}

fn satisfied(runs: &[Value], mode: WaitMode) -> bool {
    match mode {
        WaitMode::Any => runs.iter().any(terminal),
        WaitMode::All => runs.iter().all(terminal),
    }
}

fn needs_attention(runs: &[Value]) -> bool {
    runs.iter()
        .any(|run| run["needs_user_input"] == true || run["has_pending_question"] == true)
}

fn response(runs: Vec<Value>, mode: WaitMode, inbox_ready: bool) -> Value {
    json!({
        "timed_out": !satisfied(&runs, mode) && !needs_attention(&runs) && !inbox_ready,
        "inbox_ready": inbox_ready,
        "user_input_ready": false,
        "attention_run_ids": runs.iter().filter(|run| run["needs_user_input"] == true || run["has_pending_question"] == true).map(|run| &run["run_id"]).collect::<Vec<_>>(),
        "completed_run_ids": runs.iter().filter(|run| terminal(run)).map(|run| &run["run_id"]).collect::<Vec<_>>(),
        "runs": runs,
    })
}

fn snapshots(runtime: &AgentRuntime, caller: RunId, runs: &[RunId]) -> Result<Vec<Value>, String> {
    runs.iter()
        .map(|run| {
            let output = runtime.run_output(caller, *run).map_err(|error| {
                json!({
                    "code": if matches!(error, crate::RuntimeError::UnknownRun { .. }) { "unknown_run" } else { "wait_denied" },
                    "run_id": run.to_string(),
                    "message": error.to_string(),
                }).to_string()
            })?;
            let mut value = serde_json::to_value(output).map_err(|error| error.to_string())?;
            value["needs_user_input"] = Value::Bool(!terminal(&value) && runtime.has_pending_user_questions(*run));
            value["has_pending_question"] =
                Value::Bool(runtime.has_unanswered_questions(*run));
            for (field, limit) in [("output", OUTPUT_BYTES), ("reason", REASON_BYTES)] {
                let truncated = value[field].as_str().is_some_and(|text| text.len() > limit);
                if truncated {
                    let text = value[field].as_str().expect("string checked above");
                    let mut end = limit;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    value[field] = Value::String(text[..end].to_owned());
                }
                value[format!("{field}_truncated")] = Value::Bool(truncated);
            }
            Ok(value)
        })
        .collect()
}

#[cfg(test)]
mod tests;
