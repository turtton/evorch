//! The helper side of the protocol: everything `evorch-renderer` does except
//! drive CEF.
//!
//! [`serve`] owns the two channels. One thread reads requests, validates them
//! and queues them for a single executor thread that calls the [`Backend`]
//! in arrival order; a writer thread sends responses and events; a sender
//! thread streams frames from the [`FrameQueue`] and waits for each
//! acknowledgement. Reading never waits for the backend, so `Shutdown` and a
//! closed channel are noticed while a slow request runs, and the backend is
//! told to stop.
//!
//! The helper never supervises or restarts anything: when the control channel
//! closes, or either channel breaks, it stops its backend and returns the
//! reason. The process that owns the helper decides what happens next.

use std::collections::HashMap;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use crate::codec::{
    ACK_LIMITS, CONTROL_LIMITS, CodecError, FRAME_LIMITS, Kind, MAX_CONTROL_BODY, MAX_CONTROL_META,
    decode_meta, encode_meta, read_message, write_message,
};
use crate::frames::FrameQueue;
use crate::protocol::{
    Ack, ErrorKind, EventFrame, EventKind, FrameMeta, MAX_TABS, Notices, OpenArtifact, Outcome,
    PROTOCOL_VERSION, RemoteError, RendererGeneration, Reply, Request, RequestFrame, RequestId,
    ResponseFrame, SessionId, TabGoneReason, TabId, Viewport,
};

/// Requests that may wait for the executor. A fuller queue answers `Busy`.
pub const MAX_QUEUED_REQUESTS: usize = 8;
const OUTBOX_DEPTH: usize = 256;

/// What a renderer implementation must do. Calls arrive from the executor
/// thread, one at a time, in request order; the tab table and every bound have
/// already been checked, so a backend only renders.
pub trait Backend: Send + Sync + 'static {
    /// Opens the page described by `open`; `files` are the bundle's
    /// `(path, mime, bytes)`. Returns the viewport actually in effect.
    fn open_artifact(
        &self,
        open: &OpenArtifact,
        files: Vec<(String, String, Vec<u8>)>,
    ) -> Result<Viewport, RemoteError>;
    fn resize(&self, tab: TabId, viewport: Viewport) -> Result<(), RemoteError>;
    /// Frame delivery for `tab` was switched; when enabled the backend should
    /// repaint so the first frame does not wait for the page to change.
    fn set_streaming(&self, tab: TabId, enabled: bool) -> Result<(), RemoteError>;
    /// A PNG of the loaded page.
    fn capture(&self, tab: TabId, timeout: Duration) -> Result<Vec<u8>, RemoteError>;
    fn cdp(
        &self,
        tab: TabId,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RemoteError>;
    fn settle(&self, tab: TabId, timeout: Duration) -> Result<(), RemoteError>;
    fn take_notices(&self, tab: TabId) -> Result<Notices, RemoteError>;
    fn close_tab(&self, tab: TabId);
    /// Stops everything and wakes any call that is waiting. Called once, from
    /// the thread that called [`serve`], before the backend is dropped there.
    fn shutdown(&self);
}

/// Why [`serve`] returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// The core asked the helper to stop and was answered.
    ShutdownRequested,
    /// The core closed the control channel.
    PeerClosed,
    /// The first request never arrived in time, or was not a valid `Hello`.
    HandshakeFailed(String),
    /// The backend could not start; the core was told.
    InitFailed(String),
    /// A channel carried something the protocol forbids, or broke.
    ProtocolViolation(String),
    /// The core stopped acknowledging frames, or the frame channel broke.
    FrameChannelFailed(String),
}

#[derive(Clone, Copy, Debug)]
pub struct ServeConfig {
    /// How long the core has to send `Hello`.
    pub hello_timeout: Duration,
    /// Bounds a write, and the remainder of a message once it has begun.
    pub io_timeout: Duration,
    /// How long the core may take to acknowledge a frame.
    pub ack_timeout: Duration,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            hello_timeout: Duration::from_secs(30),
            io_timeout: Duration::from_secs(10),
            ack_timeout: Duration::from_secs(10),
        }
    }
}

struct Outgoing {
    kind: Kind,
    meta: Vec<u8>,
    body: Vec<u8>,
}

struct Shared {
    generation: AtomicU64,
    tabs: Mutex<HashMap<TabId, SessionId>>,
    queue: Arc<FrameQueue>,
    out: Mutex<Option<SyncSender<Outgoing>>>,
    exit: Mutex<Option<ExitReason>>,
    shutting_down: AtomicBool,
    control_abort: UnixStream,
    frames_abort: UnixStream,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn generation(&self) -> RendererGeneration {
        RendererGeneration(self.generation.load(Ordering::Acquire))
    }

    /// Records the first reason to stop and breaks both channels, which wakes
    /// every thread that is blocked on one of them.
    fn abort(&self, reason: ExitReason) {
        lock(&self.exit).get_or_insert(reason);
        self.queue.close();
        let _ = self.control_abort.shutdown(Shutdown::Both);
        let _ = self.frames_abort.shutdown(Shutdown::Both);
    }

    fn send(&self, kind: Kind, meta: Vec<u8>, body: Vec<u8>) {
        let result = match lock(&self.out).as_ref() {
            Some(out) => out.try_send(Outgoing { kind, meta, body }),
            None => return,
        };
        match result {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.abort(ExitReason::ProtocolViolation(
                "the core stopped reading the control channel".into(),
            )),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn respond(&self, id: RequestId, result: Result<(Reply, Vec<u8>), RemoteError>) {
        let (outcome, body) = match result {
            Ok((reply, body)) => (Outcome::Ok { reply }, body),
            Err(error) => (Outcome::Err { error }, Vec::new()),
        };
        let build = |outcome, body: Vec<u8>| {
            let frame = ResponseFrame {
                generation: self.generation(),
                id,
                outcome,
            };
            encode_meta(&frame).map(|meta| (meta, body))
        };
        let (meta, body) = match build(outcome, body) {
            Ok((meta, body))
                if meta.len() <= MAX_CONTROL_META && body.len() <= MAX_CONTROL_BODY =>
            {
                (meta, body)
            }
            // What the backend produced does not fit the channel; say so
            // instead of sending something the core would reject.
            _ => {
                let error = RemoteError::new(
                    ErrorKind::TooLarge,
                    "the result does not fit in a control message",
                );
                match build(Outcome::Err { error }, Vec::new()) {
                    Ok(parts) => parts,
                    Err(_) => return,
                }
            }
        };
        self.send(Kind::Response, meta, body);
    }

    fn reserve_tab(&self, tab: TabId, session: SessionId) -> Result<(), RemoteError> {
        let mut tabs = lock(&self.tabs);
        if tabs.contains_key(&tab) {
            return Err(RemoteError::new(
                ErrorKind::Conflict,
                format!("tab {tab} already exists"),
            ));
        }
        if tabs.len() >= MAX_TABS {
            return Err(RemoteError::new(
                ErrorKind::Busy,
                format!("at most {MAX_TABS} tabs can be open"),
            ));
        }
        tabs.insert(tab, session);
        Ok(())
    }

    fn release_tab(&self, tab: TabId) {
        lock(&self.tabs).remove(&tab);
        self.queue.set_streaming(tab, false);
    }

    fn require_tab(&self, tab: TabId) -> Result<(), RemoteError> {
        if lock(&self.tabs).contains_key(&tab) {
            Ok(())
        } else {
            Err(RemoteError::not_found(format!("tab {tab} is not open")))
        }
    }
}

/// Handed to the backend when it starts: where it publishes frames and
/// reports what happens to a page on its own.
pub struct BackendContext {
    pub frames: Arc<FrameQueue>,
    pub events: EventSink,
}

/// Lets a backend tell the core about a tab that went away without a request.
#[derive(Clone)]
pub struct EventSink {
    shared: Arc<Shared>,
}

impl EventSink {
    /// The tab is gone: forget it and tell the core.
    pub fn tab_gone(&self, tab: TabId, reason: TabGoneReason) {
        let known = lock(&self.shared.tabs).contains_key(&tab);
        self.shared.release_tab(tab);
        if !known {
            return;
        }
        let frame = EventFrame {
            generation: self.shared.generation(),
            event: EventKind::TabGone { tab, reason },
        };
        if let Ok(meta) = encode_meta(&frame) {
            self.shared.send(Kind::Event, meta, Vec::new());
        }
    }
}

struct Job {
    frame: RequestFrame,
    body: Vec<u8>,
}

/// Runs the helper side of one connection until it ends.
///
/// `init` starts the backend after a valid `Hello`; its error is sent to the
/// core as the `Hello` response. It and `Backend::shutdown` run on the
/// calling thread, which matters for CEF.
pub fn serve<B: Backend>(
    control: UnixStream,
    frames: UnixStream,
    config: ServeConfig,
    init: impl FnOnce(BackendContext) -> Result<B, RemoteError>,
) -> ExitReason {
    let (writer_stream, control_abort, frames_abort, frames_stream) = match (
        control.try_clone(),
        control.try_clone(),
        frames.try_clone(),
        frames.try_clone(),
    ) {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
        _ => return ExitReason::ProtocolViolation("cannot duplicate the channels".into()),
    };
    let (out_tx, out_rx) = sync_channel(OUTBOX_DEPTH);
    let shared = Arc::new(Shared {
        generation: AtomicU64::new(0),
        tabs: Mutex::new(HashMap::new()),
        queue: Arc::new(FrameQueue::default()),
        out: Mutex::new(Some(out_tx)),
        exit: Mutex::new(None),
        shutting_down: AtomicBool::new(false),
        control_abort,
        frames_abort,
    });
    let writer = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || write_loop(writer_stream, out_rx, &shared, config))
    };
    let (backend, reason) = run(&control, frames_stream, &shared, config, init);
    // Reached by every path: stop the senders, then wait for the writer to
    // flush what is queued (the shutdown answer, late responses).
    shared.queue.close();
    *lock(&shared.out) = None;
    let _ = writer.join();
    let _ = control.shutdown(Shutdown::Both);
    let _ = frames.shutdown(Shutdown::Both);
    drop(backend);
    lock(&shared.exit).take().unwrap_or(reason)
}

fn run<B: Backend>(
    control: &UnixStream,
    frames_stream: UnixStream,
    shared: &Arc<Shared>,
    config: ServeConfig,
    init: impl FnOnce(BackendContext) -> Result<B, RemoteError>,
) -> (Option<Arc<B>>, ExitReason) {
    // Handshake.
    let _ = control.set_read_timeout(Some(config.hello_timeout));
    let hello = match read_request(control, config) {
        Ok(job) => job,
        Err(CodecError::Timeout) => {
            return (None, ExitReason::HandshakeFailed("no Hello arrived".into()));
        }
        Err(CodecError::Closed) => return (None, ExitReason::PeerClosed),
        Err(error) => return (None, ExitReason::HandshakeFailed(error.to_string())),
    };
    let _ = control.set_read_timeout(None);
    shared
        .generation
        .store(hello.frame.generation.0, Ordering::Release);
    let checked = match &hello.frame.request {
        Request::Hello { .. } => hello.frame.request.validate(hello.body.len()),
        _ => Err(RemoteError::invalid("the first request must be Hello")),
    };
    if let Err(error) = checked {
        let reason = ExitReason::HandshakeFailed(error.message.clone());
        shared.respond(hello.frame.id, Err(error));
        return (None, reason);
    }
    let context = BackendContext {
        frames: Arc::clone(&shared.queue),
        events: EventSink {
            shared: Arc::clone(shared),
        },
    };
    let backend = match init(context) {
        Ok(backend) => Arc::new(backend),
        Err(error) => {
            let reason = ExitReason::InitFailed(error.message.clone());
            shared.respond(hello.frame.id, Err(error));
            return (None, reason);
        }
    };
    shared.respond(
        hello.frame.id,
        Ok((
            Reply::Hello {
                protocol: PROTOCOL_VERSION,
                helper: format!("evorch-renderer {}", env!("CARGO_PKG_VERSION")),
            },
            Vec::new(),
        )),
    );

    let sender = {
        let shared = Arc::clone(shared);
        std::thread::spawn(move || frame_loop(frames_stream, &shared, config))
    };
    let (job_tx, job_rx) = sync_channel::<Job>(MAX_QUEUED_REQUESTS);
    let executor = {
        let shared = Arc::clone(shared);
        let backend = Arc::clone(&backend);
        std::thread::spawn(move || execute_loop(&*backend, &shared, job_rx))
    };

    let (reason, shutdown_id) = read_loop(control, shared, config, &job_tx);

    shared.shutting_down.store(true, Ordering::Release);
    // Wakes a call the executor is waiting in; queued jobs are then answered
    // with ShuttingDown without reaching the backend.
    backend.shutdown();
    drop(job_tx);
    let _ = executor.join();
    if let Some(id) = shutdown_id {
        // Answered after everything else, so it is the last message the core
        // reads; a response to an earlier request can never follow it.
        shared.respond(id, Ok((Reply::Done, Vec::new())));
    }
    shared.queue.close();
    // Only the read half: it wakes a sender that waits for an acknowledgement,
    // while a frame that is half written is still sent whole. Cutting the write
    // side here left the core with a truncated frame, which it could not tell
    // from damage. A core that stopped reading holds this up for at most the
    // write timeout.
    let _ = shared.frames_abort.shutdown(Shutdown::Read);
    let _ = sender.join();
    (Some(backend), reason)
}

fn read_request(control: &UnixStream, config: ServeConfig) -> Result<Job, CodecError> {
    let message = read_message(control, CONTROL_LIMITS, Vec::new(), config.io_timeout)?;
    if message.kind != Kind::Request {
        return Err(CodecError::Malformed(format!(
            "expected a request, got {:?}",
            message.kind
        )));
    }
    let frame: RequestFrame = decode_meta(&message.meta)?;
    Ok(Job {
        frame,
        body: message.body,
    })
}

fn read_loop(
    control: &UnixStream,
    shared: &Arc<Shared>,
    config: ServeConfig,
    jobs: &SyncSender<Job>,
) -> (ExitReason, Option<RequestId>) {
    loop {
        let job = match read_request(control, config) {
            Ok(job) => job,
            Err(error) => {
                // A reason recorded by `abort` (frame channel, outbox) wins
                // over the shutdown it caused on this channel.
                let reason = match error {
                    CodecError::Closed => ExitReason::PeerClosed,
                    other => ExitReason::ProtocolViolation(other.to_string()),
                };
                return (lock(&shared.exit).clone().unwrap_or(reason), None);
            }
        };
        let Job { frame, body } = job;
        let id = frame.id;
        if frame.generation != shared.generation() {
            shared.respond(
                id,
                Err(RemoteError::new(
                    ErrorKind::StaleGeneration,
                    format!(
                        "request is for generation {}, this helper is generation {}",
                        frame.generation,
                        shared.generation()
                    ),
                )),
            );
            continue;
        }
        if let Request::Hello { .. } = frame.request {
            shared.respond(id, Err(RemoteError::invalid("Hello was already received")));
            continue;
        }
        if let Err(error) = frame.request.validate(body.len()) {
            shared.respond(id, Err(error));
            continue;
        }
        if let Request::Shutdown = frame.request {
            return (ExitReason::ShutdownRequested, Some(id));
        }
        match jobs.try_send(Job { frame, body }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => shared.respond(
                id,
                Err(RemoteError::new(
                    ErrorKind::Busy,
                    format!("{MAX_QUEUED_REQUESTS} requests are already waiting"),
                )),
            ),
            Err(TrySendError::Disconnected(_)) => {
                let reason = lock(&shared.exit)
                    .clone()
                    .unwrap_or_else(|| ExitReason::ProtocolViolation("executor stopped".into()));
                return (reason, None);
            }
        }
    }
}

fn execute_loop<B: Backend>(backend: &B, shared: &Shared, jobs: Receiver<Job>) {
    for Job { frame, body } in jobs {
        let result = if shared.shutting_down.load(Ordering::Acquire) {
            Err(RemoteError::new(
                ErrorKind::ShuttingDown,
                "the renderer is shutting down",
            ))
        } else {
            execute(backend, shared, frame.request, body)
        };
        shared.respond(frame.id, result);
    }
}

fn execute<B: Backend>(
    backend: &B,
    shared: &Shared,
    request: Request,
    body: Vec<u8>,
) -> Result<(Reply, Vec<u8>), RemoteError> {
    let ms = |timeout_ms: u32| Duration::from_millis(u64::from(timeout_ms));
    let done = |result: Result<(), RemoteError>| result.map(|()| (Reply::Done, Vec::new()));
    match request {
        Request::OpenArtifact(open) => {
            shared.reserve_tab(open.tab, open.session)?;
            let files = open.split_body(&body);
            match backend.open_artifact(&open, files) {
                Ok(viewport) => Ok((
                    Reply::Opened {
                        tab: open.tab,
                        viewport,
                    },
                    Vec::new(),
                )),
                Err(error) => {
                    shared.release_tab(open.tab);
                    Err(error)
                }
            }
        }
        Request::Resize { tab, viewport } => {
            shared.require_tab(tab)?;
            done(backend.resize(tab, viewport))
        }
        Request::SetStreaming { tab, enabled } => {
            shared.require_tab(tab)?;
            // Enabled first so the repaint the backend forces is accepted.
            if enabled {
                shared.queue.set_streaming(tab, true);
            }
            let result = backend.set_streaming(tab, enabled);
            if !enabled || result.is_err() {
                shared.queue.set_streaming(tab, false);
            }
            done(result)
        }
        Request::Capture { tab, timeout_ms } => {
            shared.require_tab(tab)?;
            backend
                .capture(tab, ms(timeout_ms))
                .map(|png| (Reply::Png, png))
        }
        Request::Cdp {
            tab,
            method,
            params,
            timeout_ms,
        } => {
            shared.require_tab(tab)?;
            backend
                .cdp(tab, &method, params, ms(timeout_ms))
                .map(|result| (Reply::Cdp { result }, Vec::new()))
        }
        Request::Settle { tab, timeout_ms } => {
            shared.require_tab(tab)?;
            done(backend.settle(tab, ms(timeout_ms)))
        }
        Request::TakeNotices { tab } => {
            shared.require_tab(tab)?;
            backend
                .take_notices(tab)
                .map(|notices| (Reply::Notices(notices.bounded()), Vec::new()))
        }
        Request::CloseTab { tab } => {
            shared.require_tab(tab)?;
            backend.close_tab(tab);
            shared.release_tab(tab);
            Ok((Reply::Done, Vec::new()))
        }
        Request::CloseSession { session } => {
            let tabs: Vec<TabId> = lock(&shared.tabs)
                .iter()
                .filter(|(_, owner)| **owner == session)
                .map(|(tab, _)| *tab)
                .collect();
            for tab in tabs {
                backend.close_tab(tab);
                shared.release_tab(tab);
            }
            Ok((Reply::Done, Vec::new()))
        }
        // Both are answered before a request reaches the executor.
        Request::Hello { .. } | Request::Shutdown => {
            Err(RemoteError::internal("request reached the executor twice"))
        }
    }
}

fn write_loop(
    stream: UnixStream,
    outgoing: Receiver<Outgoing>,
    shared: &Shared,
    config: ServeConfig,
) {
    let _ = stream.set_write_timeout(Some(config.io_timeout));
    for Outgoing { kind, meta, body } in outgoing.iter() {
        if let Err(error) = write_message(&stream, kind, &meta, &body, CONTROL_LIMITS) {
            shared.abort(ExitReason::ProtocolViolation(format!(
                "cannot write to the control channel: {error}"
            )));
            // Keep draining so senders never block on a dead writer.
            for _ in outgoing.iter() {}
            return;
        }
    }
}

fn frame_loop(stream: UnixStream, shared: &Shared, config: ServeConfig) {
    let _ = stream.set_write_timeout(Some(config.io_timeout));
    let _ = stream.set_read_timeout(Some(config.ack_timeout));
    let mut ack_buffer = Vec::new();
    while let Some(frame) = shared.queue.next() {
        let meta = FrameMeta {
            generation: shared.generation(),
            tab: frame.tab,
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
        };
        let fail = |what: &str, detail: String| {
            // An orderly stop closes this channel itself to wake the sender; that
            // is not a failure and must not replace the stop reason or break the
            // control channel the shutdown answer is still leaving through.
            if shared.shutting_down.load(Ordering::Acquire) {
                return;
            }
            shared.abort(ExitReason::FrameChannelFailed(format!("{what}: {detail}")));
        };
        let Ok(meta_bytes) = encode_meta(&meta) else {
            return fail("encode", "frame meta".into());
        };
        if let Err(error) =
            write_message(&stream, Kind::Frame, &meta_bytes, &frame.bgra, FRAME_LIMITS)
        {
            return fail("send", error.to_string());
        }
        let ack = match read_message(&stream, ACK_LIMITS, ack_buffer, config.io_timeout) {
            Ok(ack) => ack,
            Err(error) => return fail("acknowledgement", error.to_string()),
        };
        ack_buffer = ack.body;
        match decode_meta::<Ack>(&ack.meta) {
            Ok(Ack { seq }) if ack.kind == Kind::Ack && seq == frame.seq => {}
            _ => {
                return fail(
                    "acknowledgement",
                    format!("unexpected reply to frame {}", frame.seq),
                );
            }
        }
        shared.queue.recycle(frame.bgra);
    }
}

/// Starts [`serve`] on its own thread; for tests and embedders that do not
/// need CEF's same-thread rule.
pub fn serve_in_thread<B: Backend>(
    control: UnixStream,
    frames: UnixStream,
    config: ServeConfig,
    init: impl FnOnce(BackendContext) -> Result<B, RemoteError> + Send + 'static,
) -> JoinHandle<ExitReason> {
    std::thread::spawn(move || serve(control, frames, config, init))
}
