//! Event-driven observation of directly related runs. A wait has no deadline:
//! it returns when a target finishes or an interactive child completes a turn
//! (per mode), a target asks a question this
//! caller has not been told about, an agent message or user input arrives, or
//! the caller is cancelled. Waiting never restores or cancels the observed work.

use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Duration;

use event_bus::AgentRunPhase;
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::super::{DispatchResult, error, parse, parse_run_id, serialize, success};
use crate::agent_loop::LoopState;
use crate::mailbox::CompletionNotice;
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

struct Snapshots {
    values: Vec<Value>,
    notices: Vec<Arc<CompletionNotice>>,
    ready_turns: HashSet<String>,
}

impl Deref for Snapshots {
    type Target = [Value];
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

#[derive(Debug)]
struct WaitResult {
    value: Value,
    notices: Vec<Arc<CompletionNotice>>,
    question_ids: Vec<String>,
}

impl Deref for WaitResult {
    type Target = Value;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}
impl DerefMut for WaitResult {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
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
    /// The caller's answered questions already in its history; an answer outside
    /// this set ends the wait, and the loop injects its text afterwards.
    delivered: HashSet<String>,
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
            delivered: HashSet::new(),
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
    request.delivered = state.delivered_answer_ids();
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
        let observation = observe(runtime, caller, &request, state.channels.cancel_rx.clone());
        tokio::pin!(observation);
        loop {
            tokio::select! {
                biased;
                // Keep observation alive when discarding a withdrawn input;
                // receiving a tombstone must not interrupt the wait.
                result = &mut observation => break result,
                Some(message) = state.channels.inbox_rx.recv() => {
                    // Inject after ToolResults, never between ToolUse and ToolResult.
                    state.queue_user_message(message);
                    if state.has_pending_user_messages() {
                        break user_input_response(runtime, caller, &request);
                    }
                }
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
        // Consume only the IDs in the returned snapshot, before draining the
        // mailbox so attention and a queued notice cannot report the same ID.
        runtime.observe_questions(result.question_ids.clone());
        if deliver_messages {
            deliver(runtime, caller, result);
        } else {
            result["inbox_ready"] = Value::Bool(
                runtime
                    .run_mailbox(caller)
                    .is_ok_and(|mailbox| mailbox.has_interrupting_message()),
            );
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
    let phase_only = request.legacy
        && result["inbox_ready"] != true
        && result["answer_ready"] != true
        && result["user_input_ready"] == false
        && result["messages"].as_array().is_none_or(Vec::is_empty)
        && result["attention_run_ids"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && result["completed_run_ids"]
            .as_array()
            .is_some_and(|runs| !runs.is_empty())
        && result["runs"]
            .as_array()
            .is_some_and(|runs| runs.iter().all(terminal));
    let mut dispatch = if phase_only {
        // A phase-only legacy response does not deliver the child's output.
        success(result["runs"][0]["phase"].as_str().unwrap_or("Error"))
    } else {
        let dispatch = serialize(&result.value);
        for notice in &result.notices {
            runtime.observe_completion(notice);
        }
        dispatch
    };
    dispatch.result.progress = Some(if advanced {
        tools::Progress::Advanced
    } else {
        tools::Progress::Stalled
    });
    dispatch
}

/// Moves the caller's queued agent messages into the result. Automatic child
/// completion notices are acknowledged only when output-bearing snapshots return.
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
    _runtime: &AgentRuntime,
    request: &WaitRequest,
    result: &WaitResult,
) -> bool {
    let mut advanced = false;
    for run_id in result["completed_run_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let generation = result["runs"]
            .as_array()
            .and_then(|runs| runs.iter().find(|run| run["run_id"] == run_id))
            .and_then(|run| run["turn_generation"].as_u64());
        let key = generation.map_or_else(
            || run_id.to_owned(),
            |generation| format!("{run_id}:turn-{generation}"),
        );
        advanced |= state.budget.mark_child_completed(&key);
    }
    let asked = result
        .question_ids
        .iter()
        .any(|id| !request.reported.contains(id));
    state
        .reported_questions
        .extend(result.question_ids.iter().cloned());
    let received = result["messages"].as_array().is_some_and(|m| !m.is_empty())
        || result["inbox_ready"] == true
        || result["user_input_ready"] == true
        || result["answer_ready"] == true;
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
) -> Result<WaitResult, String> {
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
) -> Result<WaitResult, String> {
    if cancel.borrow().is_interrupted() {
        return Err("wait cancelled".into());
    }
    runtime.observe_questions(request.reported.iter().cloned());
    let mut questions = runtime.shared.question_version.subscribe();
    let mut turns = runtime.shared.turn_version.subscribe();
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
    // Every result reports answers the caller has not seen yet, whatever ended
    // the wait.
    let answer = |runs, attention, inbox_ready, timed_out| {
        let answers = runtime.undelivered_answer_ids(caller, &request.delivered);
        Ok(with_answers(
            response(
                runs,
                attention,
                inbox_ready,
                request.deadline.map(|_| timed_out),
            ),
            answers,
        ))
    };
    let answered = !runtime
        .undelivered_answer_ids(caller, &request.delivered)
        .is_empty();
    if satisfied(&initial, request.mode)
        || !attention.is_empty()
        || inbox_ready
        || answered
        || request.deadline.is_some_and(|deadline| deadline.is_zero())
    {
        let timed_out =
            !satisfied(&initial, request.mode) && attention.is_empty() && !inbox_ready && !answered;
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
                    let answered = !runtime
                        .undelivered_answer_ids(caller, &request.delivered)
                        .is_empty();
                    if !attention.is_empty() || answered {
                        let runs = snapshots(runtime, caller, &request.runs)?;
                        return answer(runs, attention, mailbox.has_interrupting_message(), false);
                    }
                }
            }
            changed = turns.changed() => {
                if changed.is_ok() {
                    let runs = snapshots(runtime, caller, &request.runs)?;
                    let attention = new_question_runs(runtime, &request.runs, &request.reported);
                    if satisfied(&runs, request.mode) || !attention.is_empty() {
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

fn satisfied(runs: &Snapshots, mode: WaitMode) -> bool {
    let ready = |run: &Value| {
        terminal(run)
            || run["run_id"]
                .as_str()
                .is_some_and(|id| runs.ready_turns.contains(id))
    };
    match mode {
        WaitMode::Any => runs.iter().any(ready),
        WaitMode::All => runs.iter().all(ready),
    }
}

/// One storage snapshot owns both attention runs and the IDs actually reported.
struct QuestionAttention {
    runs: Vec<RunId>,
    ids: Vec<String>,
}
impl QuestionAttention {
    fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}
fn new_question_runs(
    runtime: &AgentRuntime,
    runs: &[RunId],
    reported: &HashSet<String>,
) -> QuestionAttention {
    let mut attention = QuestionAttention {
        runs: Vec::new(),
        ids: Vec::new(),
    };
    for run in runs {
        let ids: Vec<_> = runtime
            .user_answers(*run)
            .unwrap_or_default()
            .into_iter()
            .filter(|q| {
                q.answer.is_none() && !reported.contains(&q.id) && !runtime.question_reported(&q.id)
            })
            .map(|q| q.id)
            .collect();
        if !ids.is_empty() {
            attention.runs.push(*run);
            attention.ids.extend(ids);
        }
    }
    attention
}

/// Marks a result that carries answers to the caller's own questions that are
/// not yet in its history. The answer text is injected once after the tool
/// result by the agent loop, never here.
fn with_answers(mut result: WaitResult, question_ids: Vec<String>) -> WaitResult {
    if !question_ids.is_empty() {
        result["answer_ready"] = Value::Bool(true);
        result["question_ids"] = json!(question_ids);
    }
    result
}

fn response(
    runs: Snapshots,
    attention: QuestionAttention,
    inbox_ready: bool,
    timed_out: Option<bool>,
) -> WaitResult {
    let Snapshots {
        values: runs,
        notices,
        ready_turns,
    } = runs;
    let mut result = json!({
        "inbox_ready": inbox_ready,
        "user_input_ready": false,
        "attention_run_ids": attention.runs.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "attention_question_ids": attention.ids,
        "completed_run_ids": runs.iter().filter(|run| terminal(run) || run["run_id"].as_str().is_some_and(|id| ready_turns.contains(id))).map(|run| &run["run_id"]).collect::<Vec<_>>(),
        "runs": runs,
    });
    if let Some(timed_out) = timed_out {
        result["timed_out"] = Value::Bool(timed_out);
    }
    WaitResult {
        question_ids: attention.ids,
        value: result,
        notices,
    }
}

fn snapshots(runtime: &AgentRuntime, caller: RunId, runs: &[RunId]) -> Result<Snapshots, String> {
    let mut notices = Vec::new();
    let mut ready_turns = HashSet::new();
    let values = runs.iter()
        .map(|run| {
            let output = runtime.run_output(caller, *run).map_err(|error| {
                json!({
                    "code": if matches!(error, crate::RuntimeError::UnknownRun { .. }) { "unknown_run" } else { "wait_denied" },
                    "run_id": run.to_string(),
                    "message": error.to_string(),
                }).to_string()
            })?;
            if let Some(notice) = &output.completion_notice {
                notices.push(notice.clone());
            }
            let ready_turn = output.completion_notice.as_ref().is_some_and(|notice| !notice.observed());
            let mut value = serde_json::to_value(output).map_err(|error| error.to_string())?;
            if ready_turn && value["status"] == "turn_completed" {
                ready_turns.insert(run.to_string());
            }
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
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Snapshots {
        values,
        notices,
        ready_turns,
    })
}

#[cfg(test)]
mod tests;
