//! Reads tool output for pull request URLs and for the end of commands that publish.
//!
//! It only listens. It never changes a tool result, a history or a prompt, and it never talks
//! to an agent. A URL becomes a *candidate* the person may confirm; a finished `git push` or
//! `gh pr …` makes the scheduler fetch soon (an asynchronous shell job counts only when the job
//! itself ended, not when its handle came back).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, PullRequestEvent, ToolEvent};
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
    /// Phase 1 refresh correlations, including read-only `gh pr` commands.
    publishing_calls: HashSet<(String, String)>,
    attributable_calls: HashSet<(String, String)>,
    /// Publishing terminal identities live only until the owning run becomes terminal.
    /// Read-only refreshes need no attribution tombstones. There is no process-wide quota.
    completed: HashSet<(String, String)>,
    /// Asynchronous jobs (run, job) started by a publishing command.
    publishing_jobs: HashSet<(String, String)>,
    attributable_jobs: HashSet<(String, String)>,
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
                    observer.attributable_calls.clear();
                    observer.attributable_jobs.clear();
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

// Refresh intentionally accepts more commands than attribution. In particular, successful
// `gh pr view` must not turn somebody else's new head into an agent push.
fn attributable_command(command: &str) -> bool {
    let Some(words) = simple_command_words(command) else {
        return false;
    };
    let mut args = words.iter().map(String::as_str);
    let program = args.next().and_then(|p| p.rsplit('/').next());
    match program {
        Some("git") => {
            while let Some(arg) = args.next() {
                match arg {
                    "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" => {
                        if args.next().is_none() {
                            return false;
                        }
                    }
                    "--no-pager" | "--no-optional-locks" | "--no-replace-objects" => {}
                    arg if arg.starts_with("-c") && arg.len() > 2
                        || arg.starts_with("-C") && arg.len() > 2
                        || arg.starts_with("--git-dir=")
                        || arg.starts_with("--work-tree=")
                        || arg.starts_with("--namespace=") => {}
                    "push" => {
                        return !args.any(|a| {
                            // Git accepts unique long-option prefixes such as --dry-r.
                            // Ambiguous dry-run prefixes are unknown as well.
                            (a.starts_with("--")
                                && a.len() > 2
                                && "--dry-run".starts_with(a.split('=').next().unwrap()))
                                || a == "--help"
                                || a.starts_with("--help=")
                                || (a.starts_with('-')
                                    && !a.starts_with("--")
                                    && a[1..].contains(['n', 'h']))
                        });
                    }
                    _ => return false,
                }
            }
            false
        }
        Some("gh") => {
            while let Some(arg) = args.next() {
                match arg {
                    "-R" | "--repo" | "--hostname" => {
                        if args.next().is_none() {
                            return false;
                        }
                    }
                    arg if arg.starts_with("--repo=") || arg.starts_with("--hostname=") => {}
                    "pr" => {
                        return args.next() == Some("create")
                            && !args.any(|a| {
                                matches!(a, "--dry-run" | "--help")
                                    || a.starts_with("--dry-run=")
                                    || a.starts_with("--help=")
                                    || (a.starts_with('-')
                                        && !a.starts_with("--")
                                        && a[1..].contains('h'))
                            });
                    }
                    _ => return false,
                }
            }
            false
        }
        _ => false,
    }
}

// A literal single-command subset, not a shell interpreter. Unknown syntax loses
// attribution rather than guessing which command a successful shell actually ran.
fn simple_command_words(command: &str) -> Option<Vec<String>> {
    let mut chars = command.trim_matches([' ', '\t', '\n', '\r']).chars();
    let (mut quote, mut present) = (None, false);
    let mut word = String::new();
    let mut words = Vec::new();
    while let Some(c) = chars.next() {
        if matches!(c, '\n' | '\r') {
            return None;
        }
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '\\' {
            let next = chars.next()?;
            if matches!(next, '\n' | '\r')
                || quote == Some('"') && !matches!(next, '\\' | '"' | '$' | '`')
            {
                return None;
            }
            word.push(next);
            present = true;
        } else if matches!(c, '$' | '`') {
            return None;
        } else if quote == Some('"') {
            if c == '"' {
                quote = None;
            } else {
                word.push(c);
            }
        } else {
            match c {
                '\'' | '"' => {
                    quote = Some(c);
                    present = true;
                }
                '|' | '&' | ';' | '(' | ')' | '{' | '}' | '<' | '>' | '#' | '*' | '?' | '['
                | ']' | '~' => return None,
                ' ' | '\t' => {
                    if present {
                        words.push(std::mem::take(&mut word));
                        present = false;
                    }
                }
                _ => {
                    word.push(c);
                    present = true;
                }
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if present {
        words.push(word);
    }
    Some(words)
}

impl Observer {
    fn started(&mut self, run: &str, call: &str, command: &str) {
        let id = (run.to_owned(), call.to_owned());
        if is_publishing_command(command) {
            bounded(&mut self.publishing_calls, id.clone());
        }
        if attributable_command(command) {
            bounded(&mut self.attributable_calls, id);
        }
    }

    async fn finished(
        &mut self,
        inner: &Arc<Inner>,
        run: &str,
        identity: String,
        publishing: Option<bool>,
    ) {
        let success = if let Some(success) = publishing {
            if !self.completed.insert((run.to_owned(), identity)) {
                return;
            }
            success
        } else {
            false
        };
        if success && let Some(thread) = inner.thread_for_run(run).await {
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
    fn forget_run(&mut self, run: &str) {
        self.completed.retain(|(id, _)| id != run);
        self.publishing_calls.retain(|(id, _)| id != run);
        self.attributable_calls.retain(|(id, _)| id != run);
        self.publishing_jobs.retain(|(id, _)| id != run);
        self.attributable_jobs.retain(|(id, _)| id != run);
        self.streams.retain(|(id, _), _| id != run);
    }
    async fn handle(&mut self, inner: &Arc<Inner>, event: &Event) {
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped,
                ..
            }) => self.forget_run(run_id),
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
                if let Some(command) = command {
                    self.started(run_id, call_id, command);
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
                    let attributable = self.attributable_jobs.remove(&stream);
                    if self.publishing_jobs.remove(&stream) {
                        self.finished(
                            inner,
                            run_id,
                            format!("job:{job_id}"),
                            attributable.then_some(job_success(status, *exit_code)),
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
        let attributable_call = self
            .attributable_calls
            .remove(&(run_id.to_owned(), call_id.to_owned()));
        let known_call = publishing_call || attributable_call;
        let job = detail.and_then(|detail| detail.get("shell_job"));
        let Some(job) = job else {
            if known_call {
                let code = detail
                    .and_then(|d| d.get("exit_code"))
                    .and_then(Value::as_i64);
                self.finished(
                    inner,
                    run_id,
                    format!("call:{call_id}"),
                    attributable_call.then_some(
                        code.map_or(!is_error, |c| c == 0)
                            && detail
                                .and_then(|d| d.get("timed_out_ms"))
                                .is_none_or(Value::is_null),
                    ),
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
            if known_call {
                bounded(&mut self.publishing_jobs, job.clone());
            }
            if attributable_call {
                bounded(&mut self.attributable_jobs, job);
            }
        } else {
            let known = self.publishing_jobs.remove(&job) || known_call;
            let attributable = self.attributable_jobs.remove(&job) || attributable_call;
            if known {
                let code = detail
                    .and_then(|d| d.get("shell_job"))
                    .and_then(|j| j.get("exit_code"))
                    .and_then(Value::as_i64);
                self.finished(
                    inner,
                    run_id,
                    format!("job:{job_id}"),
                    attributable.then_some(status == "completed" && code == Some(0)),
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
        observer.started(run, call, "git push origin feature");
    }
    #[test]
    fn attribution_accepts_literal_quoted_pushes_but_not_read_only_or_ambiguous_shells() {
        for command in [
            "git push",
            "git -c 'credential.helper=!gh auth git-credential' push origin feature",
            "git -c \"credential.helper=!gh auth git-credential\" -C '/a b' push",
            "/usr/bin/git --work-tree='/a b' push origin feature",
            "gh -R owner/repo pr create --title 'a && b' --body ''",
        ] {
            assert!(attributable_command(command), "{command}");
        }
        for command in [
            "gh pr view",
            "gh pr checks",
            "gh pr list",
            "git --help push",
            "git push --dry-run",
            "git push -nq",
            "git\u{a0}push",
            "git push *",
            "gh pr create --help",
            "gh pr create --dry-run",
            "gh pr create --dry-run=true",
            "gh pr create --help=true",
            "git push && echo done",
            "git push | tee output",
            "(git push)",
            "git push; false",
            "git push $(remote)",
            "git push `remote`",
            "git -c 'unclosed push",
            "git push\ntrue",
            "git -c",
        ] {
            assert!(!attributable_command(command), "{command}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn read_only_pr_commands_still_refresh_without_crediting_sync_or_job_success() {
        let world = World::new();
        let process = world.process(enabled());
        let permit = process.permit("t");
        process.environment.bind("run", "t");
        let mut observer = Observer::default();
        for (seq, command, expected) in [
            (1, "gh pr view", false),
            (2, "gh pr checks", false),
            (3, "git push && echo done", false),
            (
                4,
                "git -c 'credential.helper=!gh auth git-credential' push",
                true,
            ),
            (5, "gh pr create", true),
        ] {
            let call = format!("call-{seq}");
            observer.started("run", &call, command);
            observer
                .shell_returned(
                    process.service.inner(),
                    "run",
                    &call,
                    Some(&json!({"exit_code":0})),
                    false,
                )
                .await;
            assert_eq!(is_agent(&process, &permit, seq), expected, "{command}");
            assert!(process.service.inner().lock().forced.remove("t"));
        }
        observer.started("run", "job-call", "gh pr list");
        observer
            .shell_returned(
                process.service.inner(),
                "run",
                "job-call",
                Some(&json!({"shell_job":{"job_id":"read-job","status":"running"}})),
                false,
            )
            .await;
        observer
            .tool(
                process.service.inner(),
                &ToolEvent::ShellJobOutput {
                    job_uid: None,
                    job_id: "read-job".into(),
                    call_id: Some("job-call".into()),
                    run_id: Some("run".into()),
                    offset: 0,
                    chunk: String::new(),
                    status: "completed".into(),
                    exit_code: Some(0),
                },
            )
            .await;
        assert!(!is_agent(&process, &permit, 6));
        assert!(process.service.inner().lock().forced.contains("t"));
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
    #[test]
    fn dry_run_abbreviations_never_grant_publishing_attribution() {
        assert!(!attributable_command("git push --dry-r origin feature"));
        // Conservatively reject even ambiguous prefixes, not just Git's unique --dr... forms.
        for len in 3..="--dry-run".len() {
            let option = &"--dry-run"[..len];
            for suffix in ["", "=true"] {
                let command = format!("git push {option}{suffix} origin feature");
                assert!(!attributable_command(&command), "{command}");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn many_completions_do_not_permanently_disable_publishing_attribution() {
        for command in ["gh pr view", "git push"] {
            let world = World::new();
            let process = world.process(enabled());
            let permit = process.permit("t");
            process.environment.bind("run", "t");
            let mut observer = Observer::default();
            for seq in 1..=u64::try_from(MAX_TRACKED + 1).unwrap() {
                let call = format!("call-{seq}");
                observer.started("run", &call, command);
                observer
                    .shell_returned(
                        process.service.inner(),
                        "run",
                        &call,
                        Some(&json!({"exit_code":0})),
                        false,
                    )
                    .await;
                // Each adopted head consumes this interval; old successes cannot mask the bug.
                let _ = is_agent(&process, &permit, seq);
            }
            if command == "gh pr view" {
                assert!(
                    observer.completed.is_empty(),
                    "read-only refreshes need no tombstones"
                );
            }
            start(&mut observer, "run", "new-push");
            observer
                .shell_returned(
                    process.service.inner(),
                    "run",
                    "new-push",
                    Some(&json!({"exit_code":0})),
                    false,
                )
                .await;
            let seq = u64::try_from(MAX_TRACKED + 2).unwrap();
            assert!(is_agent(&process, &permit, seq), "after {command}");
            // Replaying even the full start/complete pair within the run is not a new push.
            start(&mut observer, "run", "new-push");
            observer
                .shell_returned(
                    process.service.inner(),
                    "run",
                    "new-push",
                    Some(&json!({"exit_code":0})),
                    false,
                )
                .await;
            assert!(!is_agent(&process, &permit, seq + 1));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_runs_release_correlations_without_accepting_late_job_success() {
        use event_bus::{AgentRunPhase, LifecycleEvent};
        for phase in [
            AgentRunPhase::Done,
            AgentRunPhase::Error,
            AgentRunPhase::Stopped,
        ] {
            let world = World::new();
            let process = world.process(enabled());
            let permit = process.permit("t");
            process.environment.bind("run", "t");
            let mut observer = Observer::default();
            start(&mut observer, "run", "done");
            observer
                .shell_returned(
                    process.service.inner(),
                    "run",
                    "done",
                    Some(&json!({"exit_code":0})),
                    false,
                )
                .await;
            assert!(is_agent(&process, &permit, 1));
            start(&mut observer, "run", "job");
            observer
                .shell_returned(
                    process.service.inner(),
                    "run",
                    "job",
                    Some(&json!({"shell_job":{"job_id":"j","status":"running"}})),
                    false,
                )
                .await;
            // Cleanup must not destroy another still-active run's dedup state.
            observer.started("other", "pending", "git push");
            observer
                .handle(
                    process.service.inner(),
                    &Event::new(LifecycleEvent::AgentRunStateChanged {
                        run_id: "run".into(),
                        from: AgentRunPhase::Running,
                        to: phase,
                        reason: None,
                    }),
                )
                .await;
            assert!(observer.completed.iter().all(|(run, _)| run != "run"));
            assert!(
                observer
                    .attributable_calls
                    .contains(&("other".into(), "pending".into()))
            );
            observer
                .tool(
                    process.service.inner(),
                    &ToolEvent::ShellJobOutput {
                        job_uid: None,
                        job_id: "j".into(),
                        call_id: Some("job".into()),
                        run_id: Some("run".into()),
                        offset: 0,
                        chunk: String::new(),
                        status: "completed".into(),
                        exit_code: Some(0),
                    },
                )
                .await;
            assert!(
                !is_agent(&process, &permit, 2),
                "late success after {phase:?}"
            );
        }
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
