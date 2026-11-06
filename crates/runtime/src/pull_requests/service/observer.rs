//! Reads tool output for pull request URLs and for the end of commands that publish.
//!
//! It only listens. It never changes a tool result, a history or a prompt, and it never talks
//! to an agent. A URL becomes a *candidate* the person may confirm; a finished `git push` or
//! `gh pr …` makes the scheduler fetch soon (an asynchronous shell job counts only when the job
//! itself ended, not when its handle came back).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use event_bus::{Event, EventKind, PullRequestEvent, ToolEvent};
use serde_json::Value;
use storage::StorageError;
use storage::pull_requests::{
    InheritanceState, NewPullRequestCandidate, PullRequestKey, PullRequestRejection,
    PullRequestState,
};

use super::{Inner, PullRequestError};
use crate::pull_requests::candidates::{
    StreamScanner, is_command_tool, is_publishing_command, scan_text,
};
use event_bus::RecvError;

/// Most entries each table of in-flight calls and jobs keeps. A flood past it forgets all of
/// them at once; the cost is a missed early refresh or a missed chip, never a wrong one.
const MAX_TRACKED: usize = 512;

#[derive(Default)]
struct Observer {
    /// Calls whose command publishes, until they complete.
    publishing_calls: HashSet<(String, String)>,
    completed: HashSet<(String, String)>,
    attribution_saturated: bool,
    /// Asynchronous jobs (run, job) started by a publishing command.
    publishing_jobs: HashSet<(String, String)>,
    /// Output of jobs still running.
    streams: HashMap<(String, String), StreamScanner>,
}

pub(super) async fn run(inner: Arc<Inner>, mut receiver: event_bus::EventReceiver) {
    let mut shutdown = inner.shutdown.subscribe();
    let mut observer = Observer::default();
    loop {
        tokio::select! {
            event = receiver.recv() => match event {
                Ok(event) => observer.handle(&inner, &event).await,
                Err(RecvError::Lagged(_)) => {
                    // Lost correlation is never permission to attribute a later head.
                    inner.lock().attribution = Default::default();
                    observer.publishing_calls.clear();
                    observer.publishing_jobs.clear();
                }
                Err(RecvError::Closed) => break,
            },
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

fn bounded<T: std::hash::Hash + Eq>(set: &mut HashSet<T>, value: T) {
    if set.len() >= MAX_TRACKED {
        set.clear();
    }
    set.insert(value);
}

fn job_success(status: &str, code: Option<i32>) -> bool {
    status == "completed" && code == Some(0)
}

impl Observer {
    async fn finished(&mut self, inner: &Arc<Inner>, run: &str, identity: String, success: bool) {
        let id = (run.to_owned(), identity);
        if self.completed.contains(&id) {
            return;
        }
        // Do not evict dedup tombstones and turn a replay into fresh success.
        // At capacity, keep refreshing but stop attributing new heads (safe unknown).
        if self.completed.len() >= MAX_TRACKED {
            self.attribution_saturated = true;
        }
        if !self.attribution_saturated {
            self.completed.insert(id);
        }
        if success
            && !self.attribution_saturated
            && let Some(thread) = inner.thread_for_run(run).await
        {
            let ownership = Arc::clone(&inner.ownership);
            let t = thread.clone();
            if let Ok(Some(permit)) = inner.blocking(move || ownership.owned_permit(&t)).await
                && inner.check_permit(&thread, &permit).await.is_ok()
            {
                inner
                    .lock()
                    .attribution
                    .succeeded(&thread, permit.lease.generation);
            }
        }
        inner.force_run(run).await;
    }
    async fn handle(&mut self, inner: &Arc<Inner>, event: &Event) {
        match &event.kind {
            EventKind::Ownership(_) => inner.wake.notify_one(),
            EventKind::Tool(tool) if inner.config().enabled => self.tool(inner, tool).await,
            _ => {}
        }
    }

    async fn tool(&mut self, inner: &Arc<Inner>, tool: &ToolEvent) {
        match tool {
            ToolEvent::ToolStarted {
                tool_name,
                call_id,
                input,
                run_id: Some(run_id),
                ..
            } if tool_name == "shell" => {
                let command = input
                    .as_ref()
                    .and_then(|input| input.get("command"))
                    .and_then(Value::as_str);
                if command.is_some_and(is_publishing_command) {
                    bounded(
                        &mut self.publishing_calls,
                        (run_id.clone(), call_id.clone()),
                    );
                }
            }
            ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                output,
                detail,
                run_id: Some(run_id),
                parent_call_id,
                is_error,
                ..
            } => {
                if is_command_tool(tool_name)
                    && let Some(output) = output
                {
                    let source_call = parent_call_id.as_deref().unwrap_or(call_id);
                    for key in scan_text(output) {
                        inner.offer(run_id, source_call, key).await;
                    }
                }
                if tool_name == "shell" {
                    self.shell_returned(inner, run_id, call_id, detail.as_ref(), *is_error)
                        .await;
                }
            }
            ToolEvent::ShellJobOutput {
                job_id,
                call_id,
                run_id: Some(run_id),
                offset,
                chunk,
                status,
                exit_code,
                ..
            } => {
                let stream = (run_id.clone(), job_id.clone());
                if self.streams.len() >= MAX_TRACKED && !self.streams.contains_key(&stream) {
                    self.streams.clear();
                }
                self.streams
                    .entry(stream.clone())
                    .or_default()
                    .push(*offset, chunk);
                if status != "running" {
                    let source_call = call_id.as_deref().unwrap_or(job_id);
                    if let Some(scanner) = self.streams.remove(&stream) {
                        for key in scanner.finish() {
                            inner.offer(run_id, source_call, key).await;
                        }
                    }
                    if self.publishing_jobs.remove(&stream) {
                        self.finished(
                            inner,
                            run_id,
                            format!("job:{job_id}"),
                            job_success(status, *exit_code),
                        )
                        .await;
                    }
                }
            }
            _ => {}
        }
    }

    /// A shell call returned: either the command finished, or an asynchronous job was
    /// started (or polled) and the handle's status says whether it ended.
    async fn shell_returned(
        &mut self,
        inner: &Arc<Inner>,
        run_id: &str,
        call_id: &str,
        detail: Option<&Value>,
        is_error: bool,
    ) {
        let publishing_call = self
            .publishing_calls
            .remove(&(run_id.to_owned(), call_id.to_owned()));
        let job = detail.and_then(|detail| detail.get("shell_job"));
        let Some(job) = job else {
            if publishing_call {
                let code = detail
                    .and_then(|d| d.get("exit_code"))
                    .and_then(Value::as_i64);
                self.finished(
                    inner,
                    run_id,
                    format!("call:{call_id}"),
                    code.map_or(!is_error, |c| c == 0)
                        && detail
                            .and_then(|d| d.get("timed_out_ms"))
                            .is_none_or(Value::is_null),
                )
                .await;
            }
            return;
        };
        let (Some(job_id), Some(status)) = (
            job.get("job_id").and_then(Value::as_str),
            job.get("status").and_then(Value::as_str),
        ) else {
            return;
        };
        let job = (run_id.to_owned(), job_id.to_owned());
        if status == "running" {
            if publishing_call {
                bounded(&mut self.publishing_jobs, job);
            }
        } else {
            let known = self.publishing_jobs.remove(&job) || publishing_call;
            if known {
                let code = detail
                    .and_then(|d| d.get("shell_job"))
                    .and_then(|j| j.get("exit_code"))
                    .and_then(Value::as_i64);
                self.finished(
                    inner,
                    run_id,
                    format!("job:{job_id}"),
                    status == "completed" && code == Some(0),
                )
                .await;
            }
        }
    }
}

impl Inner {
    async fn thread_for_run(&self, run_id: &str) -> Option<String> {
        let (env, run) = (Arc::clone(&self.env), run_id.to_owned());
        self.blocking(move || env.thread_of_run(&run))
            .await
            .ok()
            .flatten()
    }

    /// Asks for a fetch of the pull request of the thread `run_id` belongs to.
    async fn force_run(&self, run_id: &str) {
        if let Some(thread) = self.thread_for_run(run_id).await {
            self.force(&thread);
        }
    }

    /// Offers a pull request found in the output of `source_call` to the run's thread.
    async fn offer(&self, run_id: &str, source_call: &str, key: PullRequestKey) {
        let Some(thread) = self.thread_for_run(run_id).await else {
            return;
        };
        match self
            .present_candidate(&thread, key, run_id, source_call)
            .await
        {
            Ok(()) | Err(PullRequestError::Deleted) => {}
            Err(PullRequestError::Rejected(PullRequestRejection::CandidateLimit)) => {
                tracing::debug!(thread, "too many undecided pull request candidates");
            }
            Err(error) => {
                tracing::warn!(%error, thread, "a pull request candidate was not stored");
            }
        }
    }

    async fn present_candidate(
        &self,
        thread_id: &str,
        key: PullRequestKey,
        run_id: &str,
        source_call: &str,
    ) -> Result<(), PullRequestError> {
        let state = self.thread_state(thread_id).await?;
        if state
            .active_link
            .as_ref()
            .is_some_and(|link| link.key == key)
        {
            return Ok(());
        }
        if let Some(record) = &state.inheritance
            && record.key == key
            && self.derived_from_finished(record, &key).await?
        {
            return Ok(());
        }
        let (storage, new) = (
            self.storage.clone(),
            NewPullRequestCandidate {
                thread_id: thread_id.to_owned(),
                key,
                source_run_id: run_id.to_owned(),
                source_call_id: source_call.to_owned(),
                at_ns: self.clock.now_ns(),
            },
        );
        let presented = self
            .blocking(move || storage.present_pull_request_candidate(new))
            .await??;
        if presented.created {
            self.emit(PullRequestEvent::CandidateChanged {
                thread_id: thread_id.to_owned(),
                link_revision: state.lifecycle_revision,
                transition_seq: state.transition_seq,
                pull_request: Self::reference(&presented.candidate.key),
                candidate_id: presented.candidate.candidate_id,
                state: event_bus::PullRequestCandidateState::Presented,
            });
        }
        Ok(())
    }

    /// Whether a thread derived from `record`'s parent should not be offered the very pull
    /// request it did not inherit because it is done.
    async fn derived_from_finished(
        &self,
        record: &storage::pull_requests::InheritanceRecord,
        key: &PullRequestKey,
    ) -> Result<bool, StorageError> {
        use storage::pull_requests::InheritanceReason;
        if record.state == InheritanceState::Skipped
            && record.decision_reason == Some(InheritanceReason::Merged)
        {
            return Ok(true);
        }
        let (storage, key) = (self.storage.clone(), key.clone());
        let snapshot = self
            .blocking(move || storage.pull_request_snapshot(&key))
            .await
            .map_err(|error| StorageError::Io(error.to_string()))??;
        Ok(snapshot
            .and_then(|snapshot| snapshot.facts)
            .is_some_and(|facts| facts.state == PullRequestState::Merged))
    }
}

#[cfg(test)]
mod attribution_tests {
    use super::super::tests::*;
    use super::*;
    use serde_json::json;

    fn start(observer: &mut Observer, run: &str, call: &str) {
        observer.publishing_calls.insert((run.into(), call.into()));
    }
    fn is_agent(process: &Process, permit: &crate::ownership::OwnerPermit, seq: u64) -> bool {
        let inner = process.service.inner();
        let mut state = inner.lock();
        let head = seq.to_string();
        state
            .attribution
            .adopt("t", permit.lease.generation, 1, seq, &head, Some(&head));
        state
            .attribution
            .agent("t", permit.lease.generation, 1, seq, &head)
    }
    #[tokio::test(start_paused = true)]
    async fn synchronous_exit_code_wins_over_is_error_and_missing_completion_never_succeeds() {
        let world = World::new();
        let process = world.process(enabled());
        let permit = process.permit("t");
        process.environment.bind("run", "t");
        let mut observer = Observer::default();
        for (i, detail, error, expected) in [
            (1, Some(json!({"exit_code":0})), true, true),
            (2, Some(json!({"exit_code":1})), false, false),
            (3, None, true, false),
            (4, None, false, true),
        ] {
            let call = format!("c{i}");
            start(&mut observer, "run", &call);
            observer
                .shell_returned(
                    process.service.inner(),
                    "run",
                    &call,
                    detail.as_ref(),
                    error,
                )
                .await;
            assert_eq!(is_agent(&process, &permit, i), expected);
        }
        start(&mut observer, "run", "missing");
        assert!(!is_agent(&process, &permit, 5));
        // No matching start is not evidence even with a zero exit code.
        observer
            .shell_returned(
                process.service.inner(),
                "run",
                "absent",
                Some(&json!({"exit_code":0})),
                false,
            )
            .await;
        assert!(!is_agent(&process, &permit, 6));
    }
    #[tokio::test(start_paused = true)]
    async fn jobs_require_completed_zero_and_duplicate_terminal_events_do_not_credit_the_next_head()
    {
        let world = World::new();
        let process = world.process(enabled());
        let permit = process.permit("t");
        process.environment.bind("run", "t");
        let mut o = Observer::default();
        for (i, status, exit_code) in [
            (1, "running", None),
            (2, "failed", Some(1)),
            (3, "cancelled", Some(0)),
            (4, "timed_out", None),
            (5, "completed", None),
            (6, "completed", Some(0)),
        ] {
            let call = format!("c{i}");
            start(&mut o, "run", &call);
            o.shell_returned(process.service.inner(), "run", &call,
                Some(&json!({"shell_job":{"job_id":format!("j{i}"),"status":status,"exit_code":exit_code}})), false).await;
            assert_eq!(
                is_agent(&process, &permit, i),
                status == "completed" && exit_code == Some(0)
            );
        }
        start(&mut o, "run", "c6");
        o.shell_returned(
            process.service.inner(),
            "run",
            "c6",
            Some(&json!({"shell_job":{"job_id":"j6","status":"completed","exit_code":0}})),
            false,
        )
        .await;
        assert!(!is_agent(&process, &permit, 7));
        start(&mut o, "run", "background");
        o.shell_returned(
            process.service.inner(),
            "run",
            "background",
            Some(&json!({"shell_job":{"job_id":"job","status":"running"}})),
            false,
        )
        .await;
        assert!(!is_agent(&process, &permit, 8));
        let event = ToolEvent::ShellJobOutput {
            job_uid: None,
            job_id: "job".into(),
            call_id: Some("background".into()),
            run_id: Some("run".into()),
            offset: 0,
            chunk: String::new(),
            status: "completed".into(),
            exit_code: Some(0),
        };
        o.tool(process.service.inner(), &event).await;
        assert!(is_agent(&process, &permit, 9));
        o.tool(process.service.inner(), &event).await;
        assert!(!is_agent(&process, &permit, 10));
    }
}
