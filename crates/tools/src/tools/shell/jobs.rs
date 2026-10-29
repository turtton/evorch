//! Bounded, owner-scoped shell jobs. Handles never survive process restart and
//! never replay a command.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use event_bus::{Event, EventBus, ToolEvent};
use sandbox::{Sandbox, WrappedCommand};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot, watch};

use super::io_failed;
use crate::output::PREVIEW_BYTES;
use crate::tools::shell_escalation::{ShellAccess, ShellEscalationGate};
use crate::{Progress, ToolError, ToolExecutionContext, ToolResult};

#[cfg(test)]
mod handle_tests;
mod output;
mod process;
#[cfg(all(test, target_os = "linux"))]
mod process_tests;
#[cfg(test)]
mod wait_tests;
use output::LiveOutput;
use process::{run_pipe, run_pty};

const MAX_RUNNING: usize = 8;
const MAX_RETAINED: usize = 32;
const MAX_INPUT_BYTES: usize = 16 * 1024;
/// Start and poll may wait long; stdin and stop stay short.
pub(super) const MAX_WAIT_YIELD_MS: u64 = 30 * 60 * 1000;
pub(super) const MAX_CONTROL_YIELD_MS: u64 = 60_000;
pub(super) const AUTO_YIELD_MS: u64 = 10_000;
const DEFAULT_TIMEOUT_MS: u64 = 60 * 60 * 1000;
// A poll that blocked at least this long waited for the job rather than
// immediately re-reading an unchanged result.
const MEANINGFUL_WAIT: Duration = Duration::from_millis(100);
// Live output events are display-only, so batching keeps the bus quiet
// without affecting what polls observe.
const OUTPUT_EVENT_INTERVAL: Duration = Duration::from_millis(250);
const OUTPUT_EVENT_BYTES: usize = 32 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ControlArgs {
    pub action: String,
    pub job_id: String,
    // Omitted continues after the output already returned; explicit re-reads.
    pub cursor: Option<u64>,
    pub yield_ms: Option<u64>,
    #[serde(default)]
    pub return_on: ReturnOn,
    pub input: Option<String>,
    #[serde(default)]
    pub close_stdin: bool,
    // Executor may inject cwd. It never changes an existing job's workspace.
    pub cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReturnOn {
    #[default]
    Completion,
    Output,
}

pub(super) struct JobLaunch {
    pub wrapped: WrappedCommand,
    pub interactive: bool,
    pub timeout_ms: Option<u64>,
    pub yield_ms: Option<u64>,
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
    // by_handle → allocator → job state; handle-only readers never lock jobs.
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    allocator: crate::shell_handles::HandleAllocator,
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
    owner: String,
    call_id: Option<String>,
    thread: Option<String>,
    synchronous: bool,
    /// Publishes live output although no handle is exposed: a command started inside
    /// `run_js` is waited for to its end, yet its card shows the output as it arrives.
    stream_output: bool,
    timeout_ms: Option<u64>,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    cancel: watch::Sender<bool>,
    input: mpsc::Sender<Input>,
    escalated: Option<EscalatedInput>,
}

struct State {
    output: LiveOutput,
    final_result: Option<Result<ToolResult, ToolError>>,
    exposed: bool,
    completion: Option<Completion>,
    observed: bool,
    completion_notified: bool,
    artifact: Option<serde_json::Value>,
    artifact_notice: Option<String>,
    // Stops the job's process group from any thread; it signals nothing once
    // the job's supervisor has reaped the group leader.
    process: Option<sandbox::process::ProcessGroup>,
    sandbox: Option<Arc<dyn Sandbox>>,
    // Highest cursor any response returned; omitted control cursors resume here.
    delivered_cursor: u64,
    // Output end and status of the last response, to tell a changed job from
    // an identical re-read.
    last_seen: Option<(u64, &'static str)>,
}

#[derive(Clone)]
struct Completion {
    status: &'static str,
    exit_code: Option<i32>,
    error: Option<String>,
}

impl JobRegistry {
    pub(super) fn reserve_handles(&self, next: u64) -> Result<(), ToolError> {
        self.allocator.reserve(next)
    }

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
            wrapped,
            interactive,
            timeout_ms,
            yield_ms,
            escalated,
            sandbox,
        } = launch;
        let synchronous = yield_ms.is_none();
        if let Some(ms) = yield_ms {
            validate_yield(ms, "start")?;
        }
        let auto_yield = ctx.shell_wait_policy == crate::executor::ShellWaitPolicy::AutoYield;
        // A model start may become a continuing job that polls wait on until it
        // ends, so it gets the same default lifetime as an explicit-yield job.
        let timeout_ms = timeout_ms.or((!synchronous || auto_yield).then_some(DEFAULT_TIMEOUT_MS));
        let started = tokio::time::Instant::now();
        let mut host_input = if synchronous
            && ctx.shell_wait_policy == crate::executor::ShellWaitPolicy::AutoYield
        {
            ctx.shell_input.as_ref().map(|input| input.begin_wait())
        } else {
            None
        };
        let (cancel, cancel_rx) = watch::channel(false);
        let (input, input_rx) = mpsc::channel(4);
        let (changed, _) = watch::channel(0);
        let job = {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs.values().filter(|job| job.running()).count() >= MAX_RUNNING {
                return Ok(capacity_error(&jobs, ctx, true));
            }
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
                    return Ok(capacity_error(&jobs, ctx, false));
                }
            }
            let handle = format!("job-{}", self.allocator.allocate()?);
            let job = Arc::new(Job {
                id: uuid::Uuid::new_v4().to_string(),
                handle,
                owner: ctx.run_id.clone(),
                call_id: ctx.call_id.clone(),
                thread: ctx.thread_id.clone(),
                synchronous,
                stream_output: ctx.parent_call_id.is_some(),
                timeout_ms,
                state: Mutex::new(State {
                    output: LiveOutput::new(synchronous),
                    final_result: None,
                    exposed: !synchronous,
                    completion: None,
                    observed: false,
                    completion_notified: false,
                    artifact: None,
                    artifact_notice: None,
                    process: None,
                    sandbox: Some(sandbox),
                    delivered_cursor: 0,
                    last_seen: None,
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
        let timeout = timeout_ms.map(Duration::from_millis);
        let worker = Arc::clone(&job);
        // The registry owns cancellation; the task owns the process and reaping.
        tokio::spawn(async move {
            let completion = if interactive {
                run_pty(&wrapped, &worker, input_rx, cancel_rx, timeout).await
            } else {
                run_pipe(&wrapped, &worker, input_rx, cancel_rx, timeout).await
            };
            match completion {
                Ok(completion) => worker.complete(completion),
                Err(error) => {
                    worker
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .final_result = Some(Err(error.clone()));
                    worker.complete(Completion {
                        status: "failed",
                        exit_code: None,
                        error: Some(error.to_string()),
                    });
                }
            }
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
        let reason = if let Some(ms) = yield_ms {
            job.wait(ms, None).await;
            None
        } else if ctx.shell_wait_policy == crate::executor::ShellWaitPolicy::UntilTerminal {
            job.until_terminal().await;
            None
        } else {
            tokio::select! {
                biased;
                _ = job.until_terminal() => None,
                _ = async {
                    match host_input.as_mut() {
                        Some(input) => { let _ = input.wait_for(|pending| *pending).await; if !*input.borrow() { std::future::pending::<()>().await; } }
                        None => std::future::pending::<()>().await,
                    }
                } => Some("User input arrived; control returned early while the command continues."),
                _ = tokio::time::sleep_until(started + Duration::from_millis(AUTO_YIELD_MS)) => Some("Still running after 10000 ms; control returned automatically."),
            }
        };
        // Linearize terminal-vs-handoff under one state lock. Completion that
        // won this lock returns the legacy synchronous result; otherwise the
        // continuing handle is exposed and snapshotted atomically.
        let outcome = {
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if synchronous && state.completion.is_some() {
                state.observed = true;
                state
                    .final_result
                    .take()
                    .expect("synchronous completion is finalized once")
            } else {
                state.exposed = true;
                let (mut result, _) = job.snapshot_locked(&mut state, 0, false);
                if let Some(reason) = reason {
                    result.content.push_str(&format!("\n{reason} The command was not restarted. Stdin is closed. Continue with shell action:poll, job_id:{} (it waits until the command finishes or new input arrives), or action:stop to cancel. The ten-second boundary releases this shell wait; other tools may still delay the next model request.\n", job.handle));
                }
                // Wake event forwarding only once a continuing handle is
                // exposed. Short synchronous results must not create GUI jobs.
                job.changed
                    .send_modify(|version| *version = version.wrapping_add(1));
                launch.delivered = true;
                return Ok(result);
            }
        };
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut handles = self
            .by_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.remove(&job.id);
        handles.remove(&job.handle);
        launch.delivered = true;
        outcome
    }

    pub(super) async fn control(
        &self,
        ctx: &ToolExecutionContext,
        args: ControlArgs,
    ) -> Result<ToolResult, ToolError> {
        // Poll ignores yield_ms: it waits for the job (or, with return_on:output,
        // its output) or for new input, so an unfinished job never costs a turn.
        let yield_ms = match args.action.as_str() {
            "poll" => None,
            _ => Some(args.yield_ms.unwrap_or(0)),
        };
        if let Some(ms) = yield_ms {
            validate_yield(ms, &args.action)?;
        }
        if args.action != "poll" && args.return_on != ReturnOn::Completion {
            return Err(invalid("return_on is only supported for poll"));
        }
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
                    let decision = escalated
                        .gate
                        .decide_scoped_with_cwd(
                            ctx,
                            &command,
                            &escalated.justification,
                            escalated.cwd.as_deref(),
                            escalated.access,
                        )
                        .await;
                    if let Some((reason, guidance)) = decision.denial() {
                        return Ok(ToolResult::error(format!("{reason}\n{guidance}")));
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
        let cursor = args.cursor.unwrap_or_else(|| job.delivered_cursor());
        let output_cursor =
            (args.action == "stdin" || args.return_on == ReturnOn::Output).then_some(cursor);
        let (waited, interrupted) = match yield_ms {
            Some(ms) => (job.wait(ms, output_cursor).await, false),
            None => {
                job.wait_for_event(output_cursor, ctx.shell_input.as_deref())
                    .await
            }
        };
        let (mut result, changed) = job.snapshot_preview(
            cursor,
            args.action == "poll"
                && args.cursor.is_none()
                && args.return_on == ReturnOn::Completion
                && interrupted
                && job.running(),
        );
        if args.action == "poll" {
            if interrupted && job.running() {
                result.content.push_str("\nNew input arrived; control returned while the command continues. Poll again to keep waiting.\n");
            }
            result.progress = Some(if changed || interrupted || waited >= MEANINGFUL_WAIT {
                Progress::Advanced
            } else {
                Progress::Stalled
            });
            result.idle_wait_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
        }
        Ok(result)
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
        self.drain_where(|job| job.owner == run_id).await
    }

    /// Stops the jobs a `run_js` call started (their call ids are
    /// `<parent>/<n>`) and waits for their processes to be gone. The run's other
    /// jobs are left alone.
    pub(super) async fn drain_children(
        &self,
        run_id: &str,
        parent_call_id: &str,
    ) -> Result<(), ToolError> {
        let prefix = format!("{parent_call_id}/");
        self.drain_where(|job| {
            job.owner == run_id
                && job
                    .call_id
                    .as_deref()
                    .is_some_and(|call_id| call_id.starts_with(&prefix))
        })
        .await
    }

    /// Acknowledges the outcome of the finished jobs of the `run_js` call `parent_call_id`
    /// whose child call numbers are `seqs`: the call's result named exactly these as cut
    /// off and possibly partly run, and it is in the history. A job that is still running,
    /// or whose number was not named, stays unobserved.
    pub(super) fn observe_children(&self, run_id: &str, parent_call_id: &str, seqs: &[u32]) {
        let wanted: Vec<String> = seqs
            .iter()
            .map(|seq| format!("{parent_call_id}/{seq}"))
            .collect();
        for job in self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|job| {
                job.owner == run_id
                    && job
                        .call_id
                        .as_deref()
                        .is_some_and(|call_id| wanted.iter().any(|wanted| wanted == call_id))
            })
        {
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.completion.is_some() {
                state.observed = true;
            }
        }
    }

    /// One line per job of the run that is running or whose result nobody has read,
    /// naming it as the model can: its handle (a command started inside `run_js`
    /// has none it can poll) and what state it is in.
    pub(super) fn unobserved_summaries(&self, run_id: &str) -> Vec<String> {
        let mut lines: Vec<String> = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|job| job.owner == run_id && job.unobserved())
            .map(|job| {
                let state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let status = state
                    .completion
                    .as_ref()
                    .map_or("running", |completion| completion.status);
                let status = if state.completion.is_some() {
                    format!("{status}, result not read")
                } else {
                    status.to_owned()
                };
                if state.exposed {
                    format!("{} ({status})", job.handle)
                } else {
                    format!(
                        "{} ({status}; started by a program, not pollable)",
                        job.handle
                    )
                }
            })
            .collect();
        lines.sort();
        lines
    }

    async fn drain_where(&self, selected: impl Fn(&Job) -> bool) -> Result<(), ToolError> {
        let jobs: Vec<_> = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|job| selected(job))
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
            "shell jobs did not terminate after process-group kill; do not clean up the workspace",
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
            if !state.exposed || state.observed || state.completion_notified {
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
}

fn capacity_error(
    jobs: &HashMap<String, Arc<Job>>,
    ctx: &ToolExecutionContext,
    running: bool,
) -> ToolResult {
    let mut visible: Vec<_> = jobs
        .values()
        .filter(|job| job.owner == ctx.run_id && job.thread == ctx.thread_id)
        .collect();
    visible.retain(|job| {
        let state = job
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if running {
            state.completion.is_none()
        } else {
            state.completion.is_some() && !state.observed
        }
    });
    visible.sort_by(|a, b| a.handle.cmp(&b.handle));
    let mut message = if running {
        format!(
            "Command not started: running shell job capacity reached ({MAX_RUNNING}/{MAX_RUNNING})."
        )
    } else {
        format!(
            "Command not started: shell result capacity reached ({MAX_RETAINED}/{MAX_RETAINED})."
        )
    };
    if visible.is_empty() {
        message.push_str(if running {
            " No running jobs capable of freeing this slot are visible to this owner; another owner must finish or stop its running jobs."
        } else {
            " No finished, unobserved results are visible to this owner. Running jobs must reach terminal and their outcomes must be observed, possibly by another owner, before retrying."
        });
    } else {
        for job in visible.into_iter().take(3) {
            if running {
                message.push_str(&format!(" Poll your running {} until it finishes, or stop it and observe its terminal outcome before retrying.", job.handle));
            } else {
                message.push_str(&format!(" Poll your finished, unobserved {} to observe its outcome and free retained capacity before retrying.", job.handle));
            }
        }
    }
    ToolResult::error(message)
}

fn invalid(detail: &str) -> ToolError {
    ToolError::InvalidArgs {
        detail: detail.into(),
    }
}
pub(super) fn validate_yield(ms: u64, action: &str) -> Result<(), ToolError> {
    let max_ms = if matches!(action, "start" | "poll") {
        MAX_WAIT_YIELD_MS
    } else {
        MAX_CONTROL_YIELD_MS
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
        let exposed = {
            let state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let visible = state.exposed || job.stream_output;
            if !visible && state.completion.is_some() {
                return;
            }
            visible
        };
        if !exposed {
            if changed.changed().await.is_err() {
                return;
            }
            continue;
        }
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
                job_uid: Some(job.id.clone()),
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
        if let Some(group) = &state.process {
            group.kill();
        }
    }
    fn register_process(&self, group: Option<sandbox::process::ProcessGroup>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .process = group;
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
        let capture = state.output.take_capture();
        let result = if self.synchronous {
            super::shell_result(
                capture,
                completion.exit_code.unwrap_or(-1),
                (completion.status == "timed_out")
                    .then_some(self.timeout_ms)
                    .flatten(),
            )
        } else {
            capture.finish()
        };
        state.artifact_notice =
            crate::output::artifact_reference(&result.content).map(str::to_owned);
        state.artifact = result.detail.clone();
        if self.synchronous && state.final_result.is_none() {
            state.final_result = Some(Ok(result));
        }
        state.completion = Some(completion);
        state.process = None;
        state.sandbox = None;
        drop(state);
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }
    async fn until_terminal(&self) {
        let mut changed = self.changed.subscribe();
        while self.running() {
            if changed.changed().await.is_err() {
                break;
            }
        }
    }
    fn delivered_cursor(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .delivered_cursor
    }
    /// Whether the job finished or, with an output cursor, retained output
    /// beyond it.
    fn reached(&self, output_cursor: Option<u64>) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.completion.is_some()
            || output_cursor
                .is_some_and(|cursor| state.output.offset + state.output.text.len() as u64 > cursor)
    }
    /// Resolves once [`Self::reached`] holds. Subscribes before checking state.
    async fn until_reached(&self, output_cursor: Option<u64>) {
        let mut changed = self.changed.subscribe();
        while !self.reached(output_cursor) {
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
    // Keep one deadline across output notifications. Start/stop wait for
    // teardown; stdin yields on retained output beyond its cursor. Returns how
    // long the call blocked.
    async fn wait(&self, ms: u64, output_cursor: Option<u64>) -> Duration {
        if ms == 0 {
            return Duration::ZERO;
        }
        let started = tokio::time::Instant::now();
        let _ = tokio::time::timeout(Duration::from_millis(ms), self.until_reached(output_cursor))
            .await;
        started.elapsed()
    }
    /// Waits without a deadline for [`Self::reached`] or for pending input.
    /// The job wins a tie with input. Returns how long the call blocked and
    /// whether input ended it.
    async fn wait_for_event(
        &self,
        output_cursor: Option<u64>,
        input: Option<&dyn crate::executor::ShellInputControl>,
    ) -> (Duration, bool) {
        let started = tokio::time::Instant::now();
        let interrupted = match input {
            Some(input) => {
                let input = input.wait_for_input();
                tokio::select! {
                    biased;
                    () = self.until_reached(output_cursor) => false,
                    () = input => !self.reached(output_cursor),
                }
            }
            None => {
                self.until_reached(output_cursor).await;
                false
            }
        };
        (started.elapsed(), interrupted)
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

    /// Output from `cursor` and the job status, plus whether this response
    /// shows output or a status not shown by previous responses for this job.
    #[cfg(test)]
    fn snapshot(&self, cursor: u64) -> (ToolResult, bool) {
        self.snapshot_preview(cursor, false)
    }
    fn snapshot_preview(&self, cursor: u64, prefer_tail: bool) -> (ToolResult, bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.snapshot_locked(&mut state, cursor, prefer_tail)
    }
    fn snapshot_locked(
        &self,
        state: &mut State,
        cursor: u64,
        prefer_tail: bool,
    ) -> (ToolResult, bool) {
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
        if prefer_tail {
            start = start.max(output.text.len().saturating_sub(PREVIEW_BYTES / 2));
            while !output.text.is_char_boundary(start) {
                start += 1;
            }
            if let Some((index, _)) = output.text[start..].match_indices('\n').rev().nth(199) {
                start += index + 1;
            }
        }
        while !output.text.is_char_boundary(start) {
            start += 1;
        }
        let skipped = available_from + start as u64 > cursor;
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
        if skipped {
            content.push_str(
                "[Earlier live output omitted or expired; re-read an explicit cursor, or use the output artifact after completion.]\n",
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
            "shell_job": {"job_id": self.handle, "job_uid": self.id, "run_id": self.owner, "status": status, "exit_code": exit_code, "cursor": next_cursor, "available_from": available_from, "output_end": total, "has_more": next_cursor < total, "output_gap": skipped, "recoverable_after_restart": false},
            "output_artifact": state.artifact.as_ref().and_then(|value| value.get("output_artifact"))
        }));
        result.is_error = matches!(status, "failed" | "timed_out" | "cancelled");
        let seen = Some((total, status));
        // Paging through retained output also delivers something new.
        let changed = state.last_seen != seen || next_cursor > state.delivered_cursor;
        state.last_seen = seen;
        state.delivered_cursor = state.delivered_cursor.max(next_cursor);
        (result, changed)
    }
}
