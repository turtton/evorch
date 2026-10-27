//! Event-driven observation of directly related runs. A wait has no deadline:
//! it returns when a target finishes (per mode), a target asks a question this
//! caller has not been told about, an agent message or user input arrives, or
//! the caller is cancelled. Waiting never restores or cancels the observed work.

use std::collections::HashSet;
use std::time::Duration;

use event_bus::AgentRunPhase;
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::super::{DispatchResult, error, parse, parse_run_id, serialize, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, RunId, RunInterrupt};

pub(in crate::meta) const MAX_WAIT_RUNS: usize = 8;
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
}

/// Removes arguments that older transcripts taught models to send and that a
/// wait now ignores, before schema validation: `timeout_ms`, from the time a
/// wait had a deadline. The recorded tool call keeps the original input.
pub(crate) fn drop_ignored_arguments(name: &str, input: &mut Value) {
    if name == "wait"
        && let Some(object) = input.as_object_mut()
    {
        object.remove("timeout_ms");
    }
}

struct WaitRequest {
    runs: Vec<RunId>,
    mode: WaitMode,
    /// Internal bound for tests and snapshots; model and delegate waits have none.
    deadline: Option<Duration>,
    legacy: bool,
    /// Question IDs already reported to the caller; only others end the wait.
    reported: HashSet<String>,
}

impl WaitArgs {
    fn validate(self) -> Result<WaitRequest, String> {
        let legacy = self.run_id.is_some() && self.run_ids.is_none() && self.mode.is_none();
        let runs = match (self.run_id, self.run_ids) {
            (Some(run), None) => vec![run],
            (None, Some(runs)) if (1..=MAX_WAIT_RUNS).contains(&runs.len()) => runs,
            (None, Some(_)) => return Err("run_ids must contain 1 to 8 runs".into()),
            _ => return Err("provide exactly one of run_id or run_ids".into()),
        };
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
            deadline: None,
            legacy,
            reported: HashSet::new(),
        })
    }
}

/// The model's `wait`: agent messages that end it are returned in the result
/// and consumed, so no separate `inbox` call is needed.
pub(in crate::meta) async fn wait(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> DispatchResult {
    wait_with(state, runtime, input, true).await
}

/// A `delegate` wait: each delegate call returns its own child's result, so an
/// agent message only sets `inbox_ready` and stays queued for `inbox`.
pub(in crate::meta) async fn wait_for_delegates(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> DispatchResult {
    wait_with(state, runtime, input, false).await
}

async fn wait_with(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: Value,
    deliver_messages: bool,
) -> DispatchResult {
    let mut request = match parse::<WaitArgs>(input).and_then(WaitArgs::validate) {
        Ok(request) => request,
        Err(message) => return error(message),
    };
    // Validate every target before suspending the parent or observing any output.
    if let Err(message) = snapshots(runtime, state.caller_run_id(), &request.runs) {
        return error(message);
    }
    request.reported.clone_from(&state.reported_questions);
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        return error("parent run could not enter Waiting");
    }
    let caller = state.caller_run_id();
    let mut result = if state.channels.cancel_rx.borrow().is_interrupted() {
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
        }
        if deliver_messages {
            deliver(runtime, caller, result);
        }
    }
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return error("parent run could not resume Running");
    }
    let result = match result {
        Ok(result) => result,
        Err(message) => return error(message),
    };
    let advanced = record_observations(state, runtime, &request, &result);
    let mut dispatch = if request.legacy
        && result["inbox_ready"] != true
        && result["user_input_ready"] == false
        && result["messages"].as_array().is_none_or(Vec::is_empty)
        && result["attention_run_ids"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && result["completed_run_ids"]
            .as_array()
            .is_some_and(|runs| !runs.is_empty())
    {
        // Preserve the established single-run success result.
        success(result["runs"][0]["phase"].as_str().unwrap_or("Error"))
    } else {
        serialize(&result)
    };
    dispatch.result.progress = Some(if advanced {
        tools::Progress::Advanced
    } else {
        tools::Progress::Stalled
    });
    dispatch
}

/// Moves the caller's queued agent messages into the result. Automatic child
/// completion notices stay queued: the run snapshots already carry them.
fn deliver(runtime: &AgentRuntime, caller: RunId, result: &mut Value) {
    let messages = match runtime.run_mailbox(caller) {
        Ok(mailbox) => mailbox.drain_interrupting(),
        Err(_) => Vec::new(),
    };
    if let Some(object) = result.as_object_mut() {
        object.remove("inbox_ready");
    }
    result["messages"] = serde_json::to_value(messages).unwrap_or_else(|_| json!([]));
}

/// Remembers the questions now reported to the caller and reports whether this
/// wait brought anything new: a first observed completion, a new question, an
/// agent message or user input. Only such waits reset the loop guards, so a
/// wait that keeps returning the same finished child is still detected.
fn record_observations(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    request: &WaitRequest,
    result: &Value,
) -> bool {
    let mut advanced = false;
    for run_id in result["completed_run_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        advanced |= state.budget.mark_child_completed(run_id);
    }
    let questions = unanswered_questions(runtime, &request.runs);
    let asked = questions.iter().any(|id| !request.reported.contains(id));
    state.reported_questions.extend(questions);
    let received = result["messages"].as_array().is_some_and(|m| !m.is_empty())
        || result["inbox_ready"] == true
        || result["user_input_ready"] == true;
    if asked || received {
        state.budget.mark_progress();
        advanced = true;
    }
    advanced
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
    let attention = new_question_runs(runtime, &request.runs, &request.reported);
    let mut result = response(runs, attention, mailbox.has_interrupting_message(), None);
    result["user_input_ready"] = Value::Bool(true);
    Ok(result)
}

async fn observe(
    runtime: &AgentRuntime,
    caller: RunId,
    request: &WaitRequest,
    mut cancel: watch::Receiver<RunInterrupt>,
) -> Result<Value, String> {
    if cancel.borrow().is_interrupted() {
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
    let attention = new_question_runs(runtime, &request.runs, &request.reported);
    let answer = |runs, attention, inbox_ready, timed_out| {
        Ok(response(
            runs,
            attention,
            inbox_ready,
            request.deadline.map(|_| timed_out),
        ))
    };
    if satisfied(&initial, request.mode)
        || !attention.is_empty()
        || inbox_ready
        || request.deadline.is_some_and(|deadline| deadline.is_zero())
    {
        let timed_out = !satisfied(&initial, request.mode) && attention.is_empty() && !inbox_ready;
        return answer(initial, attention, inbox_ready, timed_out);
    }
    // AgentRuntime::wait subscribes to each run's existing watch channel. A
    // completion between this snapshot and subscription is read immediately.
    let mut pending = request
        .runs
        .iter()
        .map(|run| runtime.wait(*run))
        .collect::<FuturesUnordered<_>>();
    let deadline = async {
        match request.deadline {
            Some(deadline) => tokio::time::sleep(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || cancel.borrow().is_interrupted() {
                    return Err("wait cancelled".into());
                }
            }
            () = &mut deadline => {
                let runs = snapshots(runtime, caller, &request.runs)?;
                let attention = new_question_runs(runtime, &request.runs, &request.reported);
                return answer(runs, attention, mailbox.has_interrupting_message(), true);
            }
            changed = inbox_changes.changed() => {
                if changed.is_ok() && mailbox.has_interrupting_message() {
                    let runs = snapshots(runtime, caller, &request.runs)?;
                    let attention = new_question_runs(runtime, &request.runs, &request.reported);
                    return answer(runs, attention, true, false);
                }
            }
            changed = questions.changed() => {
                if changed.is_ok() {
                    let attention = new_question_runs(runtime, &request.runs, &request.reported);
                    if !attention.is_empty() {
                        let runs = snapshots(runtime, caller, &request.runs)?;
                        return answer(runs, attention, mailbox.has_interrupting_message(), false);
                    }
                }
            }
            completed = pending.next() => {
                // Admission failures also become terminal snapshots; they are
                // outcomes of the target run rather than failures of this wait.
                let _ = completed;
                let runs = snapshots(runtime, caller, &request.runs)?;
                let attention = new_question_runs(runtime, &request.runs, &request.reported);
                if satisfied(&runs, request.mode) || !attention.is_empty() || pending.is_empty() {
                    return answer(runs, attention, mailbox.has_interrupting_message(), false);
                }
            }
        }
    }
}

pub(in crate::meta) fn terminal(run: &Value) -> bool {
    matches!(
        run["status"].as_str(),
        Some("completed" | "cancelled" | "failed" | "stopped")
    )
}

fn satisfied(runs: &[Value], mode: WaitMode) -> bool {
    match mode {
        WaitMode::Any => runs.iter().any(terminal),
        WaitMode::All => runs.iter().all(terminal),
    }
}

/// IDs of the targets' unanswered questions, blocking or not.
fn unanswered_questions(runtime: &AgentRuntime, runs: &[RunId]) -> Vec<String> {
    runs.iter()
        .filter_map(|run| runtime.user_answers(*run).ok())
        .flatten()
        .filter(|question| question.answer.is_none())
        .map(|question| question.id)
        .collect()
}

/// Targets with an unanswered question that was not yet reported to the caller.
fn new_question_runs(
    runtime: &AgentRuntime,
    runs: &[RunId],
    reported: &HashSet<String>,
) -> Vec<RunId> {
    runs.iter()
        .copied()
        .filter(|run| {
            runtime.user_answers(*run).is_ok_and(|questions| {
                questions
                    .iter()
                    .any(|question| question.answer.is_none() && !reported.contains(&question.id))
            })
        })
        .collect()
}

fn response(
    runs: Vec<Value>,
    attention: Vec<RunId>,
    inbox_ready: bool,
    timed_out: Option<bool>,
) -> Value {
    let mut result = json!({
        "inbox_ready": inbox_ready,
        "user_input_ready": false,
        "attention_run_ids": attention.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "completed_run_ids": runs.iter().filter(|run| terminal(run)).map(|run| &run["run_id"]).collect::<Vec<_>>(),
        "runs": runs,
    });
    if let Some(timed_out) = timed_out {
        result["timed_out"] = Value::Bool(timed_out);
    }
    result
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
