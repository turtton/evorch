//! Observation only: consumes bus metadata and output, never touches JobRegistry.
use super::*;
use event_bus::{AgentRunPhase, LifecycleEvent as BusLifecycle, ToolEvent};
use std::time::Duration;
use tokio::time::Instant;
use workbench_protocol::namespaces::execution::shell::*;

pub(super) const OUTPUT_INTERVAL: Duration = Duration::from_millis(250);
type Key = (String, String);
#[derive(Default)]
pub(super) struct ShellJobs {
    jobs: BTreeMap<Key, Job>,
    starts: BTreeMap<Key, String>,
    // The forwarder may emit its first output after a synchronous call returns.
    // Keep only a bounded recent command correlation, never a visible job.
    completed_starts: VecDeque<Key>,
    terminal_order: VecDeque<Key>,
}
struct Job {
    view: ShellJobView,
    tail: String,
    start: u64,
    end: u64,
    sent: u64,
    emitted: Option<Instant>,
}
fn observed_state(status: &str, phase: RunPhase) -> ShellJobState {
    if status == "running"
        && matches!(
            phase,
            RunPhase::Done | RunPhase::Failed | RunPhase::Cancelled
        )
    {
        return ShellJobState::Interrupted;
    }
    state(status)
}
fn state(status: &str) -> ShellJobState {
    match status {
        "running" => ShellJobState::Running,
        "completed" => ShellJobState::Completed,
        "failed" => ShellJobState::Failed,
        "timed_out" => ShellJobState::TimedOut,
        "cancelled" => ShellJobState::Cancelled,
        _ => ShellJobState::Interrupted,
    }
}
fn boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}
impl Job {
    fn append(&mut self, offset: u64, chunk: &str) {
        let Some(end) = offset.checked_add(chunk.len() as u64) else {
            return;
        };
        if end <= self.end {
            return;
        }
        if offset > self.end {
            self.tail.clear();
            self.start = offset;
            self.end = offset;
        }
        let skip = boundary(chunk, (self.end - offset) as usize);
        // An overlap ending inside a codepoint cannot splice invalid UTF-8.
        if skip > (self.end - offset) as usize {
            self.tail.clear();
            self.start = offset + skip as u64;
        }
        self.tail.push_str(&chunk[skip..]);
        self.end = end;
        if self.tail.len() > MAX_SHELL_TAIL_BYTES {
            let drop = boundary(&self.tail, self.tail.len() - MAX_SHELL_TAIL_BYTES);
            self.tail.drain(..drop);
            self.start += drop as u64;
        }
    }
    fn flush(&mut self, now: Instant) -> Option<UiEvent> {
        if self.sent >= self.end
            || self
                .emitted
                .is_some_and(|last| now.saturating_duration_since(last) < OUTPUT_INTERVAL)
        {
            return None;
        }
        let from = self
            .sent
            .max(self.start)
            .max(self.end.saturating_sub(MAX_SHELL_OUTPUT_EVENT_BYTES as u64));
        let from = boundary(&self.tail, (from - self.start) as usize);
        let offset = self.start + from as u64;
        self.sent = self.end;
        self.emitted = Some(now);
        Some(UiEvent::Execution(ExecutionEvent::ShellJobOutput {
            thread: self.view.thread.clone(),
            run: self.view.run.clone(),
            job_uid: self.view.job_uid.clone(),
            offset,
            chunk: self.tail[from..].to_owned(),
        }))
    }
}
impl ShellJobs {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn views(&self) -> impl Iterator<Item = &ShellJobView> {
        self.terminal_order
            .iter()
            .filter_map(|key| self.jobs.get(key))
            .map(|j| &j.view)
            .chain(
                self.jobs
                    .values()
                    .filter(|j| j.view.state == ShellJobState::Running)
                    .map(|j| &j.view),
            )
    }
    pub fn flush(&mut self, now: Instant) -> Vec<UiEvent> {
        self.jobs
            .values_mut()
            .filter_map(|job| job.flush(now))
            .collect()
    }
    fn update(&mut self, mut view: ShellJobView) -> Option<UiEvent> {
        if view.validate().is_err() {
            return None;
        }
        let key = (view.run.to_string(), view.job_uid.clone());
        let changed;
        if let Some(job) = self.jobs.get_mut(&key) {
            // A delayed running detail cannot undo a terminal output/lifecycle event.
            if job.view.state != ShellJobState::Running
                && matches!(
                    view.state,
                    ShellJobState::Running | ShellJobState::Interrupted
                )
            {
                view.state = job.view.state;
                view.exit_code = job.view.exit_code;
            }
            if job.view.command.is_some() {
                view.command = job.view.command.clone();
            }
            if job.view.source_call_id.is_some() {
                view.source_call_id = job.view.source_call_id.clone();
            }
            changed = job.view != view;
            job.view = view.clone();
        } else {
            changed = true;
            self.jobs.insert(
                key.clone(),
                Job {
                    view: view.clone(),
                    tail: String::new(),
                    start: 0,
                    end: 0,
                    sent: 0,
                    emitted: None,
                },
            );
        }
        if view.state != ShellJobState::Running && !self.terminal_order.contains(&key) {
            self.terminal_order.push_back(key);
        }
        while self.terminal_order.len() > MAX_RETAINED_SHELL_JOBS {
            self.jobs.remove(&self.terminal_order.pop_front().unwrap());
        }
        changed.then_some(UiEvent::Execution(ExecutionEvent::ShellJobChanged {
            job: view,
        }))
    }
    pub fn read(&self, q: &ReadShellOutput) -> Result<ShellOutput, WorkbenchError> {
        q.validate()?;
        let job = self
            .jobs
            .get(&(q.run.to_string(), q.job_uid.clone()))
            .filter(|j| j.view.thread == q.thread)
            .ok_or_else(|| queries::error(ErrorCode::NotFound, "shell job not retained"))?;
        let from = q.offset.max(job.start).min(job.end);
        let from = boundary(&job.tail, (from - job.start) as usize);
        let mut end = (from + q.max_bytes as usize).min(job.tail.len());
        while !job.tail.is_char_boundary(end) {
            end -= 1;
        }
        Ok(ShellOutput {
            thread: q.thread.clone(),
            run: q.run.clone(),
            job_uid: q.job_uid.clone(),
            available_from: job.start,
            offset: job.start + from as u64,
            chunk: job.tail[from..end].to_owned(),
            output_end: job.end,
        })
    }
    fn interrupt(&mut self, run: &str) -> Vec<UiEvent> {
        self.starts.retain(|(r, _), _| r != run);
        self.completed_starts.retain(|(r, _)| r != run);
        let views: Vec<_> = self
            .views()
            .filter(|j| j.run.as_str() == run && j.state == ShellJobState::Running)
            .cloned()
            .collect();
        views
            .into_iter()
            .filter_map(|mut v| {
                v.state = ShellJobState::Interrupted;
                self.update(v)
            })
            .collect()
    }
    pub fn observe(
        &mut self,
        event: event_bus::Event,
        models: &Models,
        now: Instant,
    ) -> Vec<UiEvent> {
        let mut out = Vec::new();
        match event.kind {
            EventKind::Tool(ToolEvent::ToolStarted {
                run_id: Some(run),
                call_id,
                tool_name,
                input: Some(input),
                ..
            }) if tool_name == "shell" => {
                if models.runs.contains_key(&run)
                    && let Some(command) = input.get("command").and_then(|v| v.as_str())
                {
                    let mut end = command.len().min(MAX_SHELL_COMMAND_BYTES);
                    while !command.is_char_boundary(end) {
                        end -= 1;
                    }
                    self.starts
                        .insert((run, call_id), command[..end].to_owned());
                }
            }
            EventKind::Tool(ToolEvent::ToolCompleted {
                run_id: Some(run),
                call_id,
                tool_name,
                detail,
                ..
            }) if tool_name == "shell" => {
                let command = self.starts.remove(&(run.clone(), call_id.clone()));
                if let Some(detail) = detail.as_ref().and_then(|v| v.get("shell_job")) {
                    let Some(uid) = detail.get("job_uid").and_then(|v| v.as_str()) else {
                        return out;
                    };
                    let Some(handle) = detail.get("job_id").and_then(|v| v.as_str()) else {
                        return out;
                    };
                    if detail.get("run_id").and_then(|v| v.as_str()) != Some(run.as_str()) {
                        return out;
                    }
                    let Some(status) = detail.get("status").and_then(|v| v.as_str()) else {
                        return out;
                    };
                    if status != "running"
                        && !self.jobs.contains_key(&(run.clone(), uid.to_owned()))
                    {
                        if let Some(command) = command {
                            let key = (run, call_id);
                            self.starts.insert(key.clone(), command);
                            self.completed_starts.push_back(key);
                            while self.completed_starts.len() > MAX_RETAINED_SHELL_JOBS {
                                self.starts
                                    .remove(&self.completed_starts.pop_front().unwrap());
                            }
                        }
                        return out;
                    }
                    let Some(summary) = models.runs.get(&run) else {
                        return out;
                    };
                    let source_call_id = command.as_ref().map(|_| call_id);
                    let view = ShellJobView {
                        thread: summary.thread.clone(),
                        run: summary.id.clone(),
                        job_uid: uid.to_owned(),
                        handle: handle.to_owned(),
                        source_call_id,
                        command,
                        state: observed_state(status, summary.phase),
                        exit_code: detail
                            .get("exit_code")
                            .and_then(|v| v.as_i64())
                            .and_then(|v| i32::try_from(v).ok()),
                    };
                    out.extend(self.update(view));
                }
            }
            EventKind::Tool(ToolEvent::ShellJobOutput {
                run_id: Some(run),
                job_uid: Some(uid),
                job_id,
                call_id,
                offset,
                chunk,
                status,
                exit_code,
            }) => {
                let Some(summary) = models.runs.get(&run) else {
                    return out;
                };
                let command = call_id
                    .as_ref()
                    .and_then(|call| self.starts.get(&(run.clone(), call.clone())))
                    .cloned();
                let view = ShellJobView {
                    thread: summary.thread.clone(),
                    run: summary.id.clone(),
                    job_uid: uid.clone(),
                    handle: job_id,
                    source_call_id: call_id,
                    command,
                    state: observed_state(&status, summary.phase),
                    exit_code,
                };
                out.extend(self.update(view));
                if let Some(job) = self.jobs.get_mut(&(run.clone(), uid)) {
                    if let Some(call) = &job.view.source_call_id {
                        let key = (run, call.clone());
                        self.starts.remove(&key);
                        self.completed_starts.retain(|k| k != &key);
                    }
                    job.append(offset, &chunk);
                    out.extend(job.flush(now));
                }
            }
            EventKind::Lifecycle(BusLifecycle::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error,
                ..
            }) => out.extend(self.interrupt(&run_id)),
            _ => (),
        }
        out
    }
}
impl Projection {
    pub(super) fn observe_shell(&self, mut event: event_bus::Event, now: Instant) {
        // Same masking policy as activity; command text never leaves the host raw.
        if let EventKind::Tool(ToolEvent::ToolStarted {
            tool_name,
            input: Some(input),
            ..
        }) = &mut event.kind
            && tool_name == "shell"
        {
            self.activity_redactor.redact_json(input);
        }
        let mut state = self.state.lock().unwrap();
        let State {
            shell_jobs, models, ..
        } = &mut *state;
        let events = shell_jobs.observe(event, models, now);
        for event in events {
            state.publish(event, self.limits);
        }
    }
    pub(super) fn flush_shell(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        for event in state.shell_jobs.flush(now) {
            state.publish(event, self.limits);
        }
    }
}

#[cfg(test)]
#[path = "shell_jobs_tests.rs"]
mod tests;
