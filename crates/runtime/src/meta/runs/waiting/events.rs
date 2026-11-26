//! A predicate over current memory state; only selected child events fetch output.
use super::*;
use crate::meta::Observations;

pub(super) fn poll(
    runtime: &AgentRuntime,
    caller: RunId,
    request: &WaitRequest,
) -> Result<(bool, WaitResult), String> {
    poll_with(runtime, caller, request, false)
}

/// Only a result that will actually be returned may enrich events from storage.
/// Host input/internal virtual deadlines force that return even when all is partial.
pub(super) fn poll_with(
    runtime: &AgentRuntime,
    caller: RunId,
    request: &WaitRequest,
    force_return: bool,
) -> Result<(bool, WaitResult), String> {
    let sources = runtime.wait_runs(caller, &request.runs)?;
    let child_ids: Vec<_> = sources.iter().filter(|r| !r.parent).map(|r| r.id).collect();
    let attention = new_question_runs(runtime, &child_ids, &request.reported);
    let own_questions = runtime.wait_questions(caller);
    let answers: Vec<_> = own_questions
        .iter()
        .filter(|q| q.answer.is_some() && !request.delivered.contains(&q.id))
        .map(|q| q.id.clone())
        .collect();
    let mut events = Vec::new();
    let mut selected = Vec::new();
    let mut parents = Vec::new();
    for run in &sources {
        if run.parent {
            if run.terminal() {
                if !request.observed.contains(&run.parent_key()) {
                    events.push(json!({"kind":"parent_terminated","run_id":run.id.to_string(),"generation":run.notice.as_ref().map(|n| n.generation),"identity":run.parent_key()}));
                }
                // Explicit terminal targets remain inspectable after the lifecycle
                // edge was presented, without replaying that edge or parent output.
                if request.runs.contains(&run.id) || !request.observed.contains(&run.parent_key()) {
                    parents.push(json!({"run_id":run.id.to_string(),"phase":run.phase,"status":parent_status(run.phase)}));
                }
            }
        } else if run.ready() && (selected.len() < MAX_WAIT_RUNS || !request.runs.is_empty()) {
            let admission_key = format!("admission:{}", run.id);
            if request.runs.is_empty()
                && run.notice.is_none()
                && request.observed.contains(&admission_key)
            {
                continue;
            }
            let kind = if run.terminal() {
                "run_completed"
            } else {
                "turn_completed"
            };
            events.push(json!({"kind":kind,"run_id":run.id.to_string(),"generation":run.notice.as_ref().map(|n| n.generation),"identity": if run.notice.is_none() {Some(admission_key)} else {None}}));
            selected.push(run.id);
        }
    }
    // A question is also a child event, even if the child has no finished turn.
    for run in &attention.runs {
        if !selected.contains(run) && selected.len() < MAX_WAIT_RUNS {
            selected.push(*run);
        }
    }
    let attention = new_question_runs(runtime, &selected, &request.reported);
    let inbox = runtime
        .run_mailbox(caller)
        .map_err(|e| e.to_string())?
        .has_interrupting_message();
    let lanes = runtime.wait_lane_deliveries(caller);
    for id in lanes.iter().take(8) {
        events.push(json!({"kind":"lane","notification_id":id,"identity":format!("lane:{id}")}));
    }
    let executor = request.executor.clone().unwrap_or_else(|| {
        runtime
            .shared
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    });
    let jobs = executor.shell_job_snapshots(&caller.to_string());
    let mut shell_jobs = Vec::new();
    for job in jobs
        .iter()
        .filter(|job| {
            job.state == tools::ShellJobState::Terminal
                && job.teardown_confirmed
                && !job.observed
                && !request.shown_shell.contains(job.identity.id())
        })
        .take(8)
    {
        let outcome = executor
            .shell_job_result(&job.identity)
            .map_err(|e| e.to_string())?;
        let content = bounded(&outcome.result.content, OUTPUT_BYTES)?;
        events.push(json!({"kind":"shell","job_id":job.identity.id(),"call_id":job.call_id,"identity":format!("shell:{}",job.identity.id())}));
        shell_jobs.push(json!({"job_id":job.identity.id(),"call_id":job.call_id,"exit_code":job.exit_code,"is_error":outcome.result.is_error,"content":content}));
    }
    let explicit = !request.runs.is_empty();
    let complete = |id: &RunId| {
        sources
            .iter()
            .find(|r| r.id == *id)
            .is_some_and(|r| r.ready())
    };
    let satisfied = explicit
        && match request.mode {
            WaitMode::Any => request.runs.iter().any(complete),
            WaitMode::All => request.runs.iter().all(complete),
        };
    // Child completion obeys any/all; all other kinds interrupt every form.
    let interrupt = inbox
        || !answers.is_empty()
        || !attention.is_empty()
        || !lanes.is_empty()
        || !shell_jobs.is_empty()
        || events.iter().any(|e| e["kind"] == "parent_terminated");
    let ready = satisfied || (!explicit && !events.is_empty()) || interrupt;
    let live_parent = sources.iter().any(|r| r.parent && !r.terminal());
    let children = sources.iter().any(|r| {
        !r.parent
            && (!r.terminal()
                || r.notice.as_ref().map_or_else(
                    || !request.observed.contains(&format!("admission:{}", r.id)),
                    |n| !n.observed(),
                ))
    });
    let pending_question = own_questions.iter().any(|q| q.is_pending());
    let managed_lanes = !runtime.wait_lanes(caller).is_empty();
    let shell_source = jobs.iter().any(|j| {
        j.state != tools::ShellJobState::Terminal
            || !j.teardown_confirmed
            || (!j.observed && !request.shown_shell.contains(j.identity.id()))
    });
    let empty = !explicit && !children && !pending_question && !managed_lanes && !shell_source;
    // No run_output/continuation (storage-backed) read on a partial all
    // predicate, including every unrelated global version wake.
    if !ready && !empty && !force_return {
        return Ok((
            false,
            response(
                Snapshots {
                    values: Vec::new(),
                    notices: Vec::new(),
                    ready_turns: HashSet::new(),
                },
                QuestionAttention {
                    runs: Vec::new(),
                    ids: Vec::new(),
                },
                false,
                request.deadline.map(|_| false),
            ),
        ));
    }
    let mut runs = snapshots(runtime, caller, &selected)?;
    runs.values.extend(parents);
    if request.delegate {
        // Delegates need unfinished member phases, never their output/storage.
        for run in sources
            .iter()
            .filter(|r| !r.parent && !selected.contains(&r.id))
        {
            runs.values.push(json!({"run_id":run.id.to_string(),"phase":run.phase,"status":"still_running","needs_user_input":false,"has_pending_question":false}));
        }
    }
    let mut result = with_answers(
        response(runs, attention, inbox, request.deadline.map(|_| false)),
        answers.clone(),
    );
    for id in &result.question_ids {
        events
            .push(json!({"kind":"question","question_id":id,"identity":format!("question:{id}")}));
    }
    for id in answers {
        events.push(json!({"kind":"answer_ready","question_id":id}));
    }
    result["events"] = json!(events);
    result["shell_jobs"] = json!(shell_jobs);
    result["status"] = json!(if !ready && empty {
        "no_wait_sources"
    } else {
        "ready"
    });
    if !ready && empty {
        let mut reasons = vec![
            "no_active_or_unread_children",
            "no_pending_questions",
            "no_managed_lanes",
            "no_running_or_unread_shell_jobs",
        ];
        if !live_parent {
            reasons.insert(0, "no_live_parent");
        }
        result["reasons"] = json!(reasons);
        result["note"] = json!(if live_parent {
            "No event sources remain. Report completion and end this turn, or ask your parent if a decision is needed; do not repeat wait."
        } else {
            "No event sources remain. Report completion instead of repeating wait; ordinary user-input idle is handled after the final response."
        });
    }
    Ok((ready || empty, result))
}
fn parent_status(phase: AgentRunPhase) -> &'static str {
    match phase {
        AgentRunPhase::Done => "completed",
        AgentRunPhase::Stopped => "stopped",
        _ => "failed",
    }
}

/// Large content remains available as an immutable output artifact. Selection
/// precedes consumption; entries omitted by the bounded page stay in the queue.
pub(super) fn bounded(text: &str, limit: usize) -> Result<String, String> {
    if text.len() <= limit {
        return Ok(text.to_owned());
    }
    let safe = secret_guard::SecretRedactor::from_env().redact(text);
    let artifact =
        tools::output::save_file(safe.text.as_bytes(), "txt").map_err(|e| e.to_string())?;
    let suffix = format!(
        "\n[Output artifact: {}; read the remaining content there]",
        artifact.display()
    );
    let mut end = limit.saturating_sub(suffix.len()).min(safe.text.len());
    while !safe.text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(format!("{}{}", &safe.text[..end], suffix))
}

pub(super) fn messages(
    runtime: &AgentRuntime,
    caller: RunId,
    completion: bool,
) -> Result<(Vec<Value>, Vec<crate::mailbox::QueuedMessage>), String> {
    let mailbox = runtime.run_mailbox(caller).map_err(|e| e.to_string())?;
    let queued: Vec<_> = mailbox
        .recovery_messages()
        .into_iter()
        .filter(|q| completion || q.interrupts_wait())
        .take(8)
        .collect();
    let mut values = Vec::new();
    for q in &queued {
        let mut value = serde_json::to_value(&q.message).map_err(|e| e.to_string())?;
        value["content"] = json!(bounded(&q.message.content, OUTPUT_BYTES)?);
        values.push(value);
    }
    Ok((values, queued))
}

pub(super) fn deliver(
    runtime: &AgentRuntime,
    caller: RunId,
    result: &mut WaitResult,
    observations: &mut Observations,
) -> Result<(), String> {
    let (values, queued) = messages(runtime, caller, false)?;
    for message in &values {
        result["events"].as_array_mut().unwrap().push(json!({"kind":"message","message_id":message["message_id"],"run_id":message["sender_run_id"]}));
    }
    result.value.as_object_mut().unwrap().remove("inbox_ready");
    result["messages"] = json!(values);
    observations.messages = queued;
    Ok(())
}

pub(super) fn observe_events(
    state: &LoopState,
    result: &WaitResult,
    observations: &mut Observations,
) -> bool {
    let mut advanced = false;
    for event in result["events"].as_array().into_iter().flatten() {
        if let Some(id) = event["identity"].as_str() {
            advanced |= !state.budget.wait_event_observed(id);
            observations.events.push(id.into());
        }
    }
    for job in result["shell_jobs"].as_array().into_iter().flatten() {
        if let Some(snapshot) = state
            .shared
            .executor
            .shell_job_snapshots(&state.caller_run_id().to_string())
            .into_iter()
            .find(|s| job["job_id"] == s.identity.id())
        {
            observations.shell.push(snapshot.identity);
        }
    }
    advanced
}
