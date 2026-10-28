//! The loop between evorch and one worker: forwards the program's tool calls to
//! the host and the host's answers back.
//!
//! Two properties the rest of the tool relies on:
//!
//! - **Everything is bounded by the call's absolute deadline.** Messages for the
//!   worker go through a queue drained by a writer task, so a worker that never
//!   reads its stdin cannot block this loop; the deadline branch always fires and
//!   the caller then kills the worker.
//! - **Calls are scheduled like a model's own batch** (Shared calls together, a
//!   writer or shell alone) and their futures live inside this one, so ending the
//!   loop for any reason drops every unfinished call before the tool returns.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;

use super::protocol::{self, HostMessage, JsError, WorkerMessage};
use super::worker::WorkerConnection;
use super::{CALLABLE_TOOLS, ChildCall, ChildOutcome, RunJsHost, call_paths};

/// Bytes of printed output the host keeps; the executor then bounds what the
/// model sees.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// How long after its own deadline a worker may take to report before the host
/// stops waiting and kills it.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Requests tolerated beyond the call limit (each is answered with a refusal);
/// a program that keeps going after that is stopped.
const OVER_LIMIT_REQUESTS: u32 = 32;
/// Total size of the arguments a program may send in one run.
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// Largest total of the lines read from the worker and not yet handled. A single
/// line is at most [`protocol::MAX_LINE_BYTES`], so this admits at least one.
const READ_AHEAD_BYTES: usize = protocol::MAX_LINE_BYTES;
/// Replies waiting for a worker that does not read them.
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

/// One call as recorded in `detail.run_js.calls`.
pub(super) struct Recorded {
    pub seq: u32,
    pub name: String,
    pub is_error: bool,
    pub paths: Vec<String>,
}

pub(super) struct Failure {
    pub seq: u32,
    pub name: String,
    pub reason: String,
}

#[derive(Default)]
pub(super) struct Run {
    pub output: String,
    pub output_truncated: bool,
    pub calls: Vec<Recorded>,
    pub written: BTreeSet<String>,
    /// Paths of writes that were cut off, so whether they happened is unknown.
    pub uncertain: BTreeSet<String>,
    pub failures: Vec<Failure>,
    pub warnings: Vec<(u32, String, String)>,
    /// Set when the commands the program started could not be confirmed stopped.
    pub drain_error: Option<String>,
    pub limit_hit: Option<&'static str>,
}

impl Run {
    fn push_output(&mut self, text: &str) {
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.output.len());
        if text.len() <= room {
            self.output.push_str(text);
            return;
        }
        self.output_truncated = true;
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.output.push_str(&text[..end]);
    }

    fn record_outcome(&mut self, seq: u32, name: String, outcome: &ChildOutcome) {
        self.written.extend(outcome.written.iter().cloned());
        self.calls.push(Recorded {
            seq,
            name: name.clone(),
            is_error: outcome.is_error,
            paths: outcome.paths.clone(),
        });
        if outcome.is_error {
            let reason = outcome
                .failure
                .clone()
                .unwrap_or_else(|| "failed".to_owned());
            self.failures.push(Failure {
                seq,
                name: name.clone(),
                reason,
            });
        }
        if let Some(warning) = &outcome.warning {
            self.warnings.push((seq, name, warning.clone()));
        }
    }

    fn record_refused(&mut self, seq: u32, name: String, reason: String) {
        self.calls.push(Recorded {
            seq,
            name: name.clone(),
            is_error: true,
            paths: Vec::new(),
        });
        self.failures.push(Failure { seq, name, reason });
    }
}

pub(super) enum Ending {
    Done {
        value: Value,
        error: Option<JsError>,
        timed_out: bool,
    },
    Failed(String),
}

/// What the loop needs to know about the call it serves.
pub(super) struct Session<'a> {
    pub host: &'a Arc<dyn RunJsHost>,
    pub call_id: &'a str,
    pub tools: &'a [String],
    pub max_tool_calls: u32,
    pub deadline: Instant,
    pub bounds: Bounds,
    /// Tests end the wait for the worker when this is notified, once they know the
    /// worker is in the state under test, instead of waiting for the clock.
    #[cfg(test)]
    pub expire: Option<Arc<tokio::sync::Notify>>,
}

/// Limits that keep a worker from making the host buffer without end.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bounds {
    pub over_limit_requests: u32,
    pub request_bytes: usize,
    pub pending_reply_bytes: usize,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            over_limit_requests: OVER_LIMIT_REQUESTS,
            request_bytes: MAX_REQUEST_BYTES,
            pending_reply_bytes: MAX_PENDING_BYTES,
        }
    }
}

/// Queues messages for the writer task, counting the bytes still unwritten.
struct Outbox {
    queue: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<AtomicUsize>,
    limit: usize,
}

enum SendError {
    Closed,
    Backlog,
}

impl Outbox {
    fn send(&self, message: &HostMessage) -> Result<(), SendError> {
        let line = protocol::encode_message(message).map_err(|_| SendError::Closed)?;
        let total = self.pending.fetch_add(line.len(), Ordering::SeqCst) + line.len();
        if total > self.limit {
            return Err(SendError::Backlog);
        }
        self.queue.send(line).map_err(|_| SendError::Closed)
    }
}

fn send_failure(error: &SendError) -> Ending {
    Ending::Failed(match error {
        SendError::Closed => "cannot reach the JS worker".to_owned(),
        SendError::Backlog => "the JS worker is not reading its replies".to_owned(),
    })
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type ChildFuture = Pin<Box<dyn Future<Output = (u64, u32, ChildOutcome)> + Send>>;

struct Pending {
    id: u64,
    seq: u32,
    name: String,
    args: Value,
}

struct InFlight {
    name: String,
    paths: Vec<String>,
}

/// Starts calls in program order: a Shared call may start while only Shared calls
/// run, any other call only when nothing runs, and nothing jumps the queue.
#[derive(Default)]
struct Scheduler {
    queue: VecDeque<Pending>,
    running: FuturesUnordered<ChildFuture>,
    in_flight: HashMap<u32, InFlight>,
    alone: Option<u32>,
}

impl Scheduler {
    fn start_ready(&mut self, session: &Session<'_>) {
        while let Some(front) = self.queue.front() {
            let shares = session.host.shares_wave(&front.name);
            let may_start = if shares {
                self.alone.is_none()
            } else {
                self.in_flight.is_empty()
            };
            if !may_start {
                break;
            }
            let Some(Pending {
                id,
                seq,
                name,
                args,
            }) = self.queue.pop_front()
            else {
                break;
            };
            if !shares {
                self.alone = Some(seq);
            }
            self.in_flight.insert(
                seq,
                InFlight {
                    name: name.clone(),
                    paths: call_paths(&name, &args),
                },
            );
            let host = Arc::clone(session.host);
            let call = ChildCall {
                parent_call_id: session.call_id.to_owned(),
                seq,
                name,
                args,
                remaining: session.deadline.saturating_duration_since(Instant::now()),
            };
            self.running.push(Box::pin(async move {
                let outcome = host.call(call).await;
                (id, seq, outcome)
            }));
        }
    }

    fn finished(&mut self, seq: u32) -> Option<InFlight> {
        if self.alone == Some(seq) {
            self.alone = None;
        }
        self.in_flight.remove(&seq)
    }

    /// Drops every call that has not finished, in flight or not yet started.
    /// Dropping an in-flight call is what makes the host close its events.
    fn cancel(&mut self, state: &mut Run) {
        drop(std::mem::take(&mut self.running));
        let mut cancelled: Vec<_> = std::mem::take(&mut self.in_flight).into_iter().collect();
        cancelled.sort_by_key(|(seq, _)| *seq);
        for (seq, call) in cancelled {
            if matches!(call.name.as_str(), "edit" | "write") {
                state.uncertain.extend(call.paths.iter().cloned());
            }
            state.calls.push(Recorded {
                seq,
                name: call.name.clone(),
                is_error: true,
                paths: call.paths,
            });
            state.failures.push(Failure {
                seq,
                name: call.name,
                reason: "cut off before it finished".to_owned(),
            });
        }
        for call in std::mem::take(&mut self.queue) {
            state.record_refused(
                call.seq,
                call.name,
                "never started: run_js ended first".to_owned(),
            );
        }
        self.alone = None;
    }
}

async fn wait_for_deadline(session: &Session<'_>, hard_deadline: Instant) {
    #[cfg(test)]
    if let Some(expire) = &session.expire {
        tokio::select! {
            () = tokio::time::sleep_until(hard_deadline) => {}
            () = expire.notified() => {}
        }
        return;
    }
    #[cfg(not(test))]
    let _ = session;
    tokio::time::sleep_until(hard_deadline).await;
}

/// Runs the program to its end. Every call still unfinished at that point is
/// dropped before this returns.
pub(super) async fn serve(
    connection: &mut WorkerConnection,
    session: Session<'_>,
    start: HostMessage,
    state: &mut Run,
) -> Ending {
    let (Some(mut reader), Some(mut writer)) = (connection.take_reader(), connection.take_writer())
    else {
        return Ending::Failed("the JS worker's pipes are unavailable".into());
    };
    let (queue, mut writes) = mpsc::unbounded_channel::<Vec<u8>>();
    let pending = Arc::new(AtomicUsize::new(0));
    let outbox = Outbox {
        queue,
        pending: Arc::clone(&pending),
        limit: session.bounds.pending_reply_bytes,
    };
    let _writer_task = AbortOnDrop(tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(line) = writes.recv().await {
            let written = writer.write_all(&line).await;
            pending.fetch_sub(line.len(), Ordering::SeqCst);
            if written.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    }));
    // Reading whole lines is not cancel-safe inside select!, so a task owns the reader.
    // What it has read but the loop has not yet handled is bounded by bytes (not only
    // by count): each message holds permits for its line until it is handled.
    let (incoming, mut messages) = mpsc::channel(64);
    let read_ahead = Arc::new(Semaphore::new(READ_AHEAD_BYTES));
    let _reader_task = AbortOnDrop(tokio::spawn(async move {
        loop {
            let message = protocol::read_message_sized::<WorkerMessage, _>(&mut reader).await;
            let last = !matches!(message, Ok(Some(_)));
            let (message, permit) = match message {
                Ok(Some((message, bytes))) => {
                    let wanted = u32::try_from(bytes.clamp(1, READ_AHEAD_BYTES)).unwrap_or(1);
                    let Ok(permit) = Arc::clone(&read_ahead).acquire_many_owned(wanted).await
                    else {
                        break;
                    };
                    (Ok(Some((message, bytes))), Some(permit))
                }
                Ok(None) => (Ok(None), None),
                Err(error) => (Err(error), None),
            };
            if incoming.send((message, permit)).await.is_err() || last {
                break;
            }
        }
    }));
    if let Err(error) = outbox.send(&start) {
        return send_failure(&error);
    }

    let mut scheduler = Scheduler::default();
    let mut seq = 0u32;
    let mut request_bytes = 0usize;
    let hard_deadline = session.deadline + KILL_GRACE;
    let ending = loop {
        tokio::select! {
            message = messages.recv() => {
                let (message, _permit) = message.unwrap_or((Ok(None), None));
                let (message, line_bytes) = match message {
                    Ok(Some(message)) => message,
                    Ok(None) => break Ending::Failed("the JS worker exited without finishing".into()),
                    Err(error) => break Ending::Failed(format!("the JS worker sent an invalid message: {}", super::summary::one_line(&error.to_string(), 200))),
                };
                match message {
                    WorkerMessage::Output { text } => state.push_output(&text),
                    WorkerMessage::Done { value, error, timed_out } => {
                        break Ending::Done { value, error, timed_out };
                    }
                    WorkerMessage::ToolCall { id, name, args } => {
                        // Only a name run_js knows is kept or answered: an arbitrary one
                        // from an untrusted worker is neither copied nor repeated.
                        let Some(name) = CALLABLE_TOOLS.iter().find(|known| **known == name).map(|known| (*known).to_owned()) else {
                            break Ending::Failed("the JS worker called a tool that run_js does not provide".to_owned());
                        };
                        seq += 1;
                        request_bytes = request_bytes.saturating_add(line_bytes);
                        if seq > session.max_tool_calls.saturating_add(session.bounds.over_limit_requests) {
                            state.limit_hit = Some("tool_calls");
                            break Ending::Failed(format!(
                                "the program kept calling tools after the limit of {} calls",
                                session.max_tool_calls
                            ));
                        }
                        if request_bytes > session.bounds.request_bytes {
                            break Ending::Failed(format!(
                                "the program sent more than {} bytes of tool arguments",
                                session.bounds.request_bytes
                            ));
                        }
                        let refusal = if seq > session.max_tool_calls {
                            state.limit_hit = Some("tool_calls");
                            Some(format!(
                                "tool call limit of {} per run_js call exceeded",
                                session.max_tool_calls
                            ))
                        } else if !session.tools.contains(&name) {
                            Some(format!("tools.{name} is not available in this run"))
                        } else {
                            None
                        };
                        if let Some(error) = refusal {
                            state.record_refused(seq, name, error.clone());
                            let reply = HostMessage::ToolResult { id, result: None, error: Some(error) };
                            if let Err(error) = outbox.send(&reply) {
                                break send_failure(&error);
                            }
                        } else {
                            scheduler.queue.push_back(Pending { id, seq, name, args });
                            scheduler.start_ready(&session);
                        }
                    }
                }
            }
            Some((id, finished, outcome)) = scheduler.running.next(), if !scheduler.running.is_empty() => {
                let name = scheduler.finished(finished).map(|call| call.name).unwrap_or_default();
                state.record_outcome(finished, name, &outcome);
                let reply = match outcome.result {
                    Ok(result) => HostMessage::ToolResult { id, result: Some(result), error: None },
                    Err(error) => HostMessage::ToolResult { id, result: None, error: Some(error) },
                };
                if let Err(error) = outbox.send(&reply) {
                    break send_failure(&error);
                }
                scheduler.start_ready(&session);
            }
            () = wait_for_deadline(&session, hard_deadline) => {
                break Ending::Done { value: Value::Null, error: None, timed_out: true };
            }
        }
    };
    scheduler.cancel(state);
    ending
}
