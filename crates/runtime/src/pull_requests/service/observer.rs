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
    publishing_calls: HashSet<String>,
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
                Err(RecvError::Lagged(_)) => {}
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

impl Observer {
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
                ..
            } if tool_name == "shell" => {
                let command = input
                    .as_ref()
                    .and_then(|input| input.get("command"))
                    .and_then(Value::as_str);
                if command.is_some_and(is_publishing_command) {
                    bounded(&mut self.publishing_calls, call_id.clone());
                }
            }
            ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                output,
                detail,
                run_id: Some(run_id),
                parent_call_id,
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
                    self.shell_returned(inner, run_id, call_id, detail.as_ref())
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
                        inner.force_run(run_id).await;
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
    ) {
        let publishing_call = self.publishing_calls.remove(call_id);
        let job = detail.and_then(|detail| detail.get("shell_job"));
        let Some(job) = job else {
            if publishing_call {
                inner.force_run(run_id).await;
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
        } else if publishing_call || self.publishing_jobs.remove(&job) {
            inner.force_run(run_id).await;
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
