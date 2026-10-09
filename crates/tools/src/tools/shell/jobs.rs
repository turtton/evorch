//! Bounded, owner-scoped shell jobs. Handles never survive process restart and
//! never replay a command. The runtime transfers its workspace mutation lease
//! to a running job, then releases it only after the child has been reaped.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use event_bus::{Event, EventBus, ToolEvent};
use sandbox::{Sandbox, WrappedCommand};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot, watch};

use super::io_failed;
use crate::output::PREVIEW_BYTES;
use crate::tools::shell_escalation::{EscalationDecision, ShellAccess, ShellEscalationGate};
use crate::{ShellJobSummary, ToolError, ToolExecutionContext, ToolResult};

#[cfg(test)]
mod handle_tests;
mod output;
mod process;
#[cfg(test)]
mod wait_tests;
use output::LiveOutput;
use process::{run_pipe, run_pty};

const MAX_RUNNING: usize = 8;
const MAX_RETAINED: usize = 32;
const MAX_INPUT_BYTES: usize = 16 * 1024;
pub(super) const MAX_YIELD_MS: u64 = 60_000;
pub(super) const MAX_POLL_YIELD_MS: u64 = 30 * 60 * 1000;
const DEFAULT_TIMEOUT_MS: u64 = 60 * 60 * 1000;
// Live output events are display-only, so batching keeps the bus quiet
// without affecting what polls observe.
const OUTPUT_EVENT_INTERVAL: Duration = Duration::from_millis(250);
const OUTPUT_EVENT_BYTES: usize = 32 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ControlArgs {
    pub action: String,
    pub job_id: String,
    #[serde(default)]
    pub cursor: u64,
    #[serde(default)]
    pub yield_ms: u64,
    pub input: Option<String>,
    #[serde(default)]
    pub close_stdin: bool,
    // Executor may inject cwd. It never changes an existing job's workspace.
    pub cwd: Option<String>,
}

pub(super) struct JobLaunch {
    pub command: String,
    pub wrapped: WrappedCommand,
    pub interactive: bool,
    pub timeout_ms: Option<u64>,
    pub yield_ms: u64,
    pub escalated: Option<EscalatedInput>,
    pub sandbox: Arc<dyn Sandbox>,
}

pub(super) struct EscalatedInput {
    pub gate: Arc<dyn ShellEscalationGate>,
    pub command: String,
    pub justification: String,
    pub cwd: Option<std::path::PathBuf>,
    pub access: ShellAccess,
}

#[derive(Default)]
pub(super) struct JobRegistry {
    // Writers hold jobs across updates to both indexes. Lock order is jobs →
    // next_handle → by_handle → job state; handle-only readers never lock jobs.
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    next_handle: Mutex<u64>,
    by_handle: Mutex<HashMap<String, Arc<Job>>>,
    events: RwLock<Option<Arc<EventBus>>>,
}

impl Drop for JobRegistry {
    fn drop(&mut self) {
        for job in self
            .jobs
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            job.cancel_now();
        }
    }
}

struct Input {
    bytes: Vec<u8>,
    close: bool,
    result: oneshot::Sender<Result<(), String>>,
}

struct Job {
    id: String,
    handle: String,
    command: String,
    owner: String,
    call_id: Option<String>,
    thread: Option<String>,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    cancel: watch::Sender<bool>,
    input: mpsc::Sender<Input>,
    escalated: Option<EscalatedInput>,
}

struct State {
    output: LiveOutput,
    completion: Option<Completion>,
    observed: bool,
    completion_notified: bool,
    artifact: Option<serde_json::Value>,
    artifact_notice: Option<String>,
    // Dropped on terminal completion, not when the starting tool returns.
    guard: Option<Box<dyn Send + Sync>>,
    process_id: Option<rustix::process::Pid>,
    sandbox: Option<Arc<dyn Sandbox>>,
}

#[derive(Clone)]
struct Completion {
    status: &'static str,
    exit_code: Option<i32>,
    error: Option<String>,
}

impl JobRegistry {
    pub(super) fn set_event_bus(&self, bus: Arc<EventBus>) {
        *self
            .events
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bus);
    }

    pub(super) async fn start(
        &self,
        ctx: &ToolExecutionContext,
        launch: JobLaunch,
    ) -> Result<ToolResult, ToolError> {
        let JobLaunch {
            command,
            wrapped,
            interactive,
            timeout_ms,
            yield_ms,
            escalated,
            sandbox,
        } = launch;
        if ctx.run_id.is_empty() {
            return Err(invalid("asynchronous shell requires a nonempty run_id"));
        }
        validate_yield(yield_ms, "start")?;
        let (cancel, cancel_rx) = watch::channel(false);
        let (input, input_rx) = mpsc::channel(4);
        let (changed, _) = watch::channel(0);
        let job = {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs.values().filter(|job| job.running()).count() >= MAX_RUNNING {
                return Ok(ToolResult::error(
                    "shell job capacity reached; stop or finish an existing job before starting another",
                ));
            }
            let mut next_handle = self
                .next_handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut by_handle = self
                .by_handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs.len() >= MAX_RETAINED {
                let removable = jobs
                    .iter()
                    .filter(|(_, job)| !job.unobserved())
                    .map(|(id, _)| id.clone())
                    .min();
                if let Some(id) = removable {
                    if let Some(removed) = jobs.remove(&id) {
                        by_handle.remove(&removed.handle);
                    }
                } else {
                    return Ok(ToolResult::error(
                        "shell result capacity reached; poll a finished job to observe its outcome before starting another",
                    ));
                }
            }
            let handle = format!("job-{}", *next_handle);
            *next_handle = next_handle
                .checked_add(1)
                .ok_or_else(|| invalid("shell job handle capacity exhausted"))?;
            let job = Arc::new(Job {
                id: uuid::Uuid::new_v4().to_string(),
                handle,
                command,
                owner: ctx.run_id.clone(),
                call_id: ctx.call_id.clone(),
                thread: ctx.thread_id.clone(),
                state: Mutex::new(State {
                    output: LiveOutput::default(),
                    completion: None,
                    observed: false,
                    completion_notified: false,
                    artifact: None,
                    artifact_notice: None,
                    guard: None,
                    process_id: None,
                    sandbox: Some(sandbox),
                }),
                changed,
                cancel,
                input,
                escalated,
            });
            jobs.insert(job.id.clone(), Arc::clone(&job));
            by_handle.insert(job.handle.clone(), Arc::clone(&job));
            job
        };
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let worker = Arc::clone(&job);
        // The registry owns cancellation; the task owns the process and reaping.
        tokio::spawn(async move {
            let completion = if interactive {
                run_pty(&wrapped, &worker, input_rx, cancel_rx, timeout).await
            } else {
                run_pipe(&wrapped, &worker, input_rx, cancel_rx, timeout).await
            };
            worker.complete(completion.unwrap_or_else(|error| Completion {
                status: "failed",
                exit_code: None,
                error: Some(error.to_string()),
            }));
        });
        let events = self
            .events
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(bus) = events {
            tokio::spawn(forward_output(Arc::clone(&job), bus));
        }
        // Cancelling the starting tool before the handle is delivered must not
        // orphan its process. Later control calls do not own process lifetime.
        let mut launch = LaunchGuard {
            job: Arc::clone(&job),
            delivered: false,
        };
        job.wait_for_completion(yield_ms).await;
        let result = job.snapshot(0);
        launch.delivered = true;
        Ok(result)
    }

    pub(super) async fn control(
        &self,
        ctx: &ToolExecutionContext,
        args: ControlArgs,
    ) -> Result<ToolResult, ToolError> {
        validate_yield(args.yield_ms, &args.action)?;
        let _ = args.cwd; // Existing jobs retain their original wrapped command.
        let job = self.by_handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&args.job_id)
            .filter(|job| job.owner == ctx.run_id && job.thread == ctx.thread_id).cloned()
            .ok_or_else(|| invalid("shell job is unavailable for this run (expired, restarted, or a different owner); commands are never automatically replayed"))?;
        match args.action.as_str() {
            "poll" => {
                if args.input.is_some() || args.close_stdin {
                    return Err(invalid("poll does not accept input or close_stdin"));
                }
            }
            "stop" => {
                if args.input.is_some() || args.close_stdin {
                    return Err(invalid("stop does not accept input or close_stdin"));
                }
                job.cancel_now();
            }
            "stdin" => {
                let input = args.input.unwrap_or_default();
                if input.len() > MAX_INPUT_BYTES {
                    return Err(invalid("stdin input exceeds 16 KiB"));
                }
                if !job.running() {
                    return Err(invalid("cannot write stdin to a finished shell job"));
                }
                // Approval for launching a program is not approval for arbitrary
                // future commands fed to an unsandboxed interactive interpreter.
                if let Some(escalated) = &job.escalated {
                    let command = format!(
                        "{}\n[stdin continuation; close_stdin={}]\n{}",
                        escalated.command, args.close_stdin, input
                    );
                    if let EscalationDecision::Deny { reason } = escalated
                        .gate
                        .decide_scoped_with_cwd(
                            ctx,
                            &command,
                            &escalated.justification,
                            escalated.cwd.as_deref(),
                            escalated.access,
                        )
                        .await
                    {
                        return Ok(ToolResult::error(reason));
                    }
                }
                let (result, received) = oneshot::channel();
                job.input
                    .try_send(Input {
                        bytes: input.into_bytes(),
                        close: args.close_stdin,
                        result,
                    })
                    .map_err(|_| invalid("shell stdin is closed or busy; input was not queued"))?;
                match tokio::time::timeout(Duration::from_secs(5), received).await {
                    Ok(Ok(Ok(()))) => {}
                    Ok(Ok(Err(error))) => return Err(io_failed(error)),
                    Ok(Err(_)) => {
                        return Err(io_failed(
                            "shell stdin closed before acknowledgement; do not replay input without checking output",
                        ));
                    }
                    Err(_) => {
                        return Err(io_failed(
                            "shell stdin acknowledgement timed out; input may have been written; check output before retrying",
                        ));
                    }
                }
            }
            _ => return Err(invalid("shell action must be start, poll, stdin, or stop")),
        }
        if args.action == "stop" {
            job.wait_for_completion(args.yield_ms).await;
        } else {
            job.wait_for_change(args.cursor, args.yield_ms).await;
        }
        Ok(job.snapshot(args.cursor))
    }

    pub(super) async fn wait(
        &self,
        ctx: &ToolExecutionContext,
        job_id: &str,
    ) -> Result<(), ToolError> {
        let job = self.by_handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(job_id)
            .filter(|job| job.owner == ctx.run_id && job.thread == ctx.thread_id).cloned()
            .ok_or_else(|| invalid("shell job is unavailable for this run (expired, restarted, or a different owner); commands are never automatically replayed"))?;
        // Subscribe before checking completion so finishing during registration
        // cannot be missed. An already completed job returns immediately.
        let mut changed = job.changed.subscribe();
        while job.running() {
            changed.changed().await.map_err(|_| {
                io_failed("shell job completion signal closed before process teardown")
            })?;
        }
        Ok(())
    }

    pub(super) fn cancel(&self, run_id: &str) {
        for job in self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|job| job.owner == run_id)
        {
            job.cancel_now();
        }
    }
    pub(super) async fn drain(&self, run_id: &str) -> Result<(), ToolError> {
        let jobs: Vec<_> = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|job| job.owner == run_id)
            .cloned()
            .collect();
        for job in &jobs {
            job.cancel_now();
        }
        for _ in 0..2 {
            let waited = tokio::time::timeout(Duration::from_secs(5), async {
                for job in &jobs {
                    let mut changed = job.changed.subscribe();
                    while job.running() {
                        if changed.changed().await.is_err() {
                            break;
                        }
                    }
                }
            })
            .await;
            if waited.is_ok() && jobs.iter().all(|job| !job.running()) {
                return Ok(());
            }
            for job in &jobs {
                job.cancel_now();
            }
        }
        Err(io_failed(
            "shell jobs did not terminate after process-group kill; workspace mutation leases are still held; do not clean up the workspace",
        ))
    }

    pub(super) fn release(&self, run_id: &str) -> Result<(), ToolError> {
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if jobs
            .values()
            .any(|job| job.owner == run_id && job.running())
        {
            return Err(io_failed(
                "cannot release shell handles before process teardown",
            ));
        }
        let mut by_handle = self
            .by_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.retain(|_, job| {
            if job.owner == run_id {
                by_handle.remove(&job.handle);
                false
            } else {
                true
            }
        });
        Ok(())
    }

    pub(super) fn running(&self, run_id: &str) -> bool {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|job| job.owner == run_id && job.running())
    }
    pub(super) fn running_shell_job_summary(&self, run_id: &str) -> Option<ShellJobSummary> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|job| job.owner == run_id && job.running())
            .min_by_key(|job| {
                job.handle[4..]
                    .parse::<u64>()
                    .expect("allocated shell handle")
            })
            .map(|job| ShellJobSummary {
                handle: job.handle.clone(),
                command_summary: command_summary(&job.command),
            })
    }

    pub(super) fn unobserved(&self, run_id: &str) -> bool {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|job| job.owner == run_id && job.unobserved())
    }

    pub(super) fn take_notifications(&self, run_id: &str) -> Vec<String> {
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut notices = Vec::new();
        for job in jobs.values().filter(|job| job.owner == run_id) {
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.observed || state.completion_notified {
                continue;
            }
            if let Some(done) = &state.completion {
                let mut notice = format!(
                    "Shell job completed: {}\nstatus: {}\n",
                    job.handle, done.status
                );
                if let Some(code) = done.exit_code {
                    notice.push_str(&format!("exit_code: {code}\n"));
                }
                notice.push_str("Use shell action poll with this job_id to retrieve its output before finishing.");
                notices.push((job.handle.clone(), notice));
                state.completion_notified = true;
            }
        }
        notices.sort_by(|a, b| a.0.cmp(&b.0));
        notices.into_iter().map(|(_, notice)| notice).collect()
    }

    pub(super) fn retain_call_guard(
        &self,
        run_id: &str,
        call_id: &str,
        guard: Box<dyn Send + Sync>,
    ) {
        let job = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|job| {
                job.owner == run_id && job.call_id.as_deref() == Some(call_id) && job.running()
            })
            .cloned();
        if let Some(job) = job {
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.completion.is_none() {
                state.guard = Some(guard);
            }
        }
    }

    pub(super) fn retain_guard(&self, run_id: &str, job_id: &str, guard: Box<dyn Send + Sync>) {
        let job = self
            .by_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(job_id)
            .filter(|job| job.owner == run_id)
            .cloned();
        if let Some(job) = job {
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.completion.is_none() {
                state.guard = Some(guard);
            }
        }
    }
}

fn command_summary(command: &str) -> String {
    let mut summary = command.replace(['\r', '\n'], " ");
    let mut chars = summary.char_indices();
    if let Some((end, _)) = chars.nth(79)
        && chars.next().is_some()
    {
        summary.truncate(end);
        summary.push('…');
    }
    summary
}

fn invalid(detail: &str) -> ToolError {
    ToolError::InvalidArgs {
        detail: detail.into(),
    }
}
fn validate_yield(ms: u64, action: &str) -> Result<(), ToolError> {
    let max_ms = if action == "poll" {
        MAX_POLL_YIELD_MS
    } else {
        MAX_YIELD_MS
    };
    if ms > max_ms {
        Err(invalid(&format!(
            "yield_ms must be between 0 and {max_ms} for {action}"
        )))
    } else {
        Ok(())
    }
}
/// Publishes redacted live output until the job completes. Observers get their
/// own copy; polls keep reading from their cursor and still own observation.
async fn forward_output(job: Arc<Job>, bus: Arc<EventBus>) {
    let mut changed = job.changed.subscribe();
    let mut sent = 0;
    loop {
        loop {
            let (offset, chunk, completion) = job.output_since(sent);
            sent = offset + chunk.len() as u64;
            // A finished job stops writing, so its last event is the one that
            // reaches the end of its output.
            let done = completion.is_some() && sent == job.output_end();
            if chunk.is_empty() && !done {
                break;
            }
            bus.emit(Event::new(ToolEvent::ShellJobOutput {
                job_id: job.handle.clone(),
                call_id: job.call_id.clone(),
                run_id: Some(job.owner.clone()),
                offset,
                chunk,
                status: completion
                    .as_ref()
                    .map_or("running", |done| done.status)
                    .to_owned(),
                exit_code: completion.and_then(|done| done.exit_code),
            }));
            if done {
                return;
            }
        }
        if changed.changed().await.is_err() {
            return;
        }
        tokio::time::sleep(OUTPUT_EVENT_INTERVAL).await;
    }
}

struct LaunchGuard {
    job: Arc<Job>,
    delivered: bool,
}
impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if !self.delivered {
            self.job.cancel_now();
        }
    }
}

impl Job {
    fn cancel_now(&self) {
        self.cancel.send_replace(true);
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pid) = state.process_id {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
    fn register_process(&self, pid: Option<rustix::process::Pid>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .process_id = pid;
        let cancelled = *self.cancel.borrow();
        if cancelled {
            self.cancel_now();
        }
    }
    fn running(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completion
            .is_none()
    }
    fn unobserved(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.completion.is_none() || !state.observed
    }

    fn push(&self, stream: usize, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .output
            .push(stream, bytes);
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }
    fn complete(&self, completion: Completion) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.output.flush(0);
        state.output.flush(1);
        let result = std::mem::take(&mut state.output.capture).finish();
        state.artifact_notice =
            crate::output::artifact_reference(&result.content).map(str::to_owned);
        state.artifact = result.detail;
        state.completion = Some(completion);
        state.process_id = None;
        state.sandbox = None;
        state.guard = None;
        drop(state);
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }
    async fn wait_for_change(&self, cursor: u64, ms: u64) {
        self.wait(ms, Some(cursor)).await;
    }
    async fn wait_for_completion(&self, ms: u64) {
        self.wait(ms, None).await;
    }
    // Subscribe before inspecting state, and keep one deadline across output
    // notifications. Start/stop wait only for teardown; poll/stdin may yield on
    // retained output beyond their cursor.
    async fn wait(&self, ms: u64, output_cursor: Option<u64>) {
        if ms == 0 {
            return;
        }
        let mut changed = self.changed.subscribe();
        let _ = tokio::time::timeout(Duration::from_millis(ms), async {
            loop {
                {
                    let state = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.completion.is_some()
                        || output_cursor.is_some_and(|cursor| {
                            state.output.offset + state.output.text.len() as u64 > cursor
                        })
                    {
                        return;
                    }
                }
                if changed.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
    }
    /// Retained output from `cursor` (or the oldest retained byte), bounded to
    /// one event, with the completion observed under the same lock.
    fn output_since(&self, cursor: u64) -> (u64, String, Option<Completion>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let output = &state.output;
        let offset = cursor.max(output.offset);
        let mut start = usize::try_from(offset - output.offset)
            .unwrap_or(usize::MAX)
            .min(output.text.len());
        while !output.text.is_char_boundary(start) {
            start += 1;
        }
        let mut end = start
            .saturating_add(OUTPUT_EVENT_BYTES)
            .min(output.text.len());
        while !output.text.is_char_boundary(end) {
            end -= 1;
        }
        (
            output.offset + start as u64,
            output.text[start..end].to_owned(),
            state.completion.clone(),
        )
    }

    fn output_end(&self) -> u64 {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.output.offset + state.output.text.len() as u64
    }

    fn snapshot(&self, cursor: u64) -> ToolResult {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A kill/drain alone is not an observed outcome. Only a terminal tool
        // response acknowledges the potentially partial filesystem effects.
        if state.completion.is_some() {
            state.observed = true;
        }
        let output = &state.output;
        let available_from = output.offset;
        let total = output.offset + output.text.len() as u64;
        let mut start = cursor
            .clamp(available_from, total)
            .saturating_sub(available_from) as usize;
        while !output.text.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + PREVIEW_BYTES / 2).min(output.text.len());
        while !output.text.is_char_boundary(end) {
            end -= 1;
        }
        if let Some((index, _)) = output.text[start..end].match_indices('\n').nth(200) {
            end = start + index + 1;
        }
        let next_cursor = available_from + end as u64;
        let completion = state.completion.as_ref();
        let status = completion.map_or("running", |done| done.status);
        let exit_code = completion.and_then(|done| done.exit_code);
        let mut content = format!(
            "shell job: {}\nstatus: {status}\ncursor: {next_cursor}\n",
            self.handle
        );
        if cursor < available_from {
            content.push_str(
                "[Earlier live output expired; use the output artifact after completion.]\n",
            );
        }
        if let Some(code) = exit_code {
            content.push_str(&format!("exit_code: {code}\n"));
        }
        if let Some(error) = completion.and_then(|done| done.error.as_deref()) {
            content.push_str(&format!("{error}\n"));
        }
        content.push_str(&output.text[start..end]);
        if let Some(notice) = &state.artifact_notice {
            content.push('\n');
            content.push_str(notice);
        }
        let mut result = ToolResult::success(content).with_detail(serde_json::json!({
            "shell_job": {"job_id": self.handle, "status": status, "exit_code": exit_code, "cursor": next_cursor, "available_from": available_from, "output_end": total, "has_more": next_cursor < total, "output_gap": cursor < available_from, "recoverable_after_restart": false},
            "output_artifact": state.artifact.as_ref().and_then(|value| value.get("output_artifact"))
        }));
        result.is_error = matches!(status, "failed" | "timed_out" | "cancelled");
        result
    }
}
