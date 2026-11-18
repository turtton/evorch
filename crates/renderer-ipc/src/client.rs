//! The core side of the protocol: a synchronous client over the two channels.
//!
//! It is plain threads and `std` types, so the core can call it from
//! `spawn_blocking` (or a dedicated thread) without this crate depending on an
//! async runtime. Process ownership (spawning the helper through the
//! admission and ledger, killing its process group) is not here: the client
//! only reports, through [`CloseReason`], that the channels ended and why.

use std::collections::{HashMap, VecDeque};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::codec::{
    ACK_LIMITS, CONTROL_LIMITS, CodecError, FRAME_LIMITS, Kind, decode_meta, encode_meta,
    read_message, write_message,
};
use crate::frames::Frame;
use crate::protocol::{
    Ack, EventFrame, EventKind, FrameMeta, Notices, OpenArtifact, Outcome, PROTOCOL_VERSION,
    RemoteError, RendererGeneration, Reply, Request, RequestFrame, RequestId, ResponseFrame,
    SessionId, TabId, Viewport,
};

const MAX_QUEUED_EVENTS: usize = 256;

/// Why the channels ended. Every pending and later request fails with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// This side closed the client.
    LocalClose,
    /// The helper acknowledged `Shutdown` and ended.
    ShutdownAcknowledged,
    /// The helper acknowledged `Shutdown` but the connection was still open at
    /// the deadline, so this side closed it.
    ShutdownTimedOut,
    /// The helper closed a channel without being asked: it exited or crashed.
    PeerClosed,
    /// A channel carried something the protocol forbids, or stalled.
    ProtocolViolation(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// No response within the wait. The request may still run in the helper;
    /// its late response is discarded.
    Timeout,
    Closed(CloseReason),
    /// The helper refused or failed the request.
    Remote(RemoteError),
    /// The request cannot be sent (too large, not encodable); nothing was sent.
    Local(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "the renderer did not answer in time"),
            Self::Closed(reason) => write!(f, "the renderer connection ended: {reason:?}"),
            Self::Remote(error) => write!(f, "the renderer failed the request: {error}"),
            Self::Local(detail) => write!(f, "the request cannot be sent: {detail}"),
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Clone, Copy, Debug)]
pub struct ClientConfig {
    /// The generation the core assigned to this helper incarnation.
    pub generation: RendererGeneration,
    /// How long the helper may take to answer `Hello`, which includes starting
    /// the browser engine.
    pub hello_timeout: Duration,
    /// Bounds a write, and the remainder of a message once it has begun.
    pub io_timeout: Duration,
    /// Extra wait, beyond a request's own `timeout_ms`, for the answer to come
    /// back.
    pub reply_grace: Duration,
}

impl ClientConfig {
    pub fn new(generation: RendererGeneration) -> Self {
        Self {
            generation,
            hello_timeout: Duration::from_secs(60),
            io_timeout: Duration::from_secs(10),
            reply_grace: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperInfo {
    pub protocol: u16,
    pub helper: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientStats {
    /// Responses and events of another generation, discarded.
    pub stale_messages: u64,
    /// Responses to requests that had already timed out, discarded.
    pub late_responses: u64,
    /// Frames of another generation, discarded.
    pub stale_frames: u64,
    pub received_frames: u64,
    /// Received frames replaced before the consumer took them.
    pub dropped_frames: u64,
    /// Events discarded because the consumer did not keep up.
    pub dropped_events: u64,
}

type Response = Result<(Reply, Vec<u8>), ClientError>;

#[derive(Default)]
struct State {
    closed: Option<CloseReason>,
    tabs: HashMap<TabId, (SessionId, Option<Frame>)>,
    spare: Option<Vec<u8>>,
    events: VecDeque<EventKind>,
    stats: ClientStats,
}

struct Inner {
    generation: RendererGeneration,
    writer: Mutex<UnixStream>,
    control_abort: UnixStream,
    frames_abort: UnixStream,
    next_id: AtomicU64,
    /// Id of the `Shutdown` request, or zero before one was sent.
    shutdown_id: AtomicU64,
    shutdown_acknowledged: AtomicBool,
    pending: Mutex<HashMap<RequestId, Sender<Response>>>,
    state: Mutex<State>,
    changed: Condvar,
    config: ClientConfig,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    /// Ends the connection. The first reason wins.
    fn close(&self, reason: CloseReason) {
        {
            let mut state = lock(&self.state);
            if state.closed.is_some() {
                return;
            }
            state.closed = Some(reason.clone());
            state.tabs.clear();
            state.spare = None;
        }
        let _ = self.control_abort.shutdown(Shutdown::Both);
        let _ = self.frames_abort.shutdown(Shutdown::Both);
        let pending: Vec<_> = lock(&self.pending).drain().collect();
        for (_, reply) in pending {
            let _ = reply.send(Err(ClientError::Closed(reason.clone())));
        }
        self.changed.notify_all();
    }

    fn closed(&self) -> Option<CloseReason> {
        lock(&self.state).closed.clone()
    }

    fn peer_ended(&self, error: CodecError) {
        let reason = match error {
            CodecError::Closed if self.shutdown_acknowledged.load(Ordering::Acquire) => {
                CloseReason::ShutdownAcknowledged
            }
            CodecError::Closed => CloseReason::PeerClosed,
            other => CloseReason::ProtocolViolation(other.to_string()),
        };
        self.close(reason);
    }

    fn count(&self, update: impl FnOnce(&mut ClientStats)) {
        update(&mut lock(&self.state).stats);
    }

    fn control_loop(&self, stream: &UnixStream) {
        loop {
            let message =
                match read_message(stream, CONTROL_LIMITS, Vec::new(), self.config.io_timeout) {
                    Ok(message) => message,
                    Err(error) => return self.peer_ended(error),
                };
            let outcome = match message.kind {
                Kind::Response => self.on_response(&message.meta, message.body),
                Kind::Event => self.on_event(&message.meta),
                other => Err(CodecError::Malformed(format!(
                    "unexpected {other:?} on the control channel"
                ))),
            };
            if let Err(error) = outcome {
                return self.peer_ended(error);
            }
        }
    }

    fn on_response(&self, meta: &[u8], body: Vec<u8>) -> Result<(), CodecError> {
        let response: ResponseFrame = decode_meta(meta)?;
        if response.generation != self.generation {
            self.count(|stats| stats.stale_messages += 1);
            return Ok(());
        }
        let Some(reply) = lock(&self.pending).remove(&response.id) else {
            self.count(|stats| stats.late_responses += 1);
            return Ok(());
        };
        // The answer to `Shutdown` is the last thing the helper sends; note it
        // so the close that follows is not taken for a crash.
        if response.id.0 == self.shutdown_id.load(Ordering::Acquire)
            && matches!(response.outcome, Outcome::Ok { .. })
        {
            self.shutdown_acknowledged.store(true, Ordering::Release);
        }
        let result = match response.outcome {
            Outcome::Ok { reply } => Ok((reply, body)),
            Outcome::Err { error } => Err(ClientError::Remote(error)),
        };
        let _ = reply.send(result);
        Ok(())
    }

    fn on_event(&self, meta: &[u8]) -> Result<(), CodecError> {
        let event: EventFrame = decode_meta(meta)?;
        let mut state = lock(&self.state);
        if event.generation != self.generation {
            state.stats.stale_messages += 1;
            return Ok(());
        }
        if state.events.len() >= MAX_QUEUED_EVENTS {
            state.stats.dropped_events += 1;
        } else {
            state.events.push_back(event.event);
        }
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    fn frame_loop(&self, stream: &UnixStream) {
        loop {
            let spare = lock(&self.state).spare.take().unwrap_or_default();
            let message = match read_message(stream, FRAME_LIMITS, spare, self.config.io_timeout) {
                Ok(message) => message,
                // The helper closes this channel around the time it closes the
                // control channel (an ordered stop, a crash, a kill); the
                // connection ends when the control channel does, so the reason
                // (acknowledged shutdown or crash) is not lost to this race. A
                // close inside a frame is the same event, only mistimed. Damage
                // that is not a close still ends the connection here.
                Err(CodecError::Closed | CodecError::Truncated) => return,
                Err(error) => return self.peer_ended(error),
            };
            match self.on_frame(stream, message) {
                Ok(()) => {}
                Err(CodecError::Closed | CodecError::Truncated) => return,
                Err(error) => return self.peer_ended(error),
            }
        }
    }

    fn on_frame(
        &self,
        stream: &UnixStream,
        message: crate::codec::Message,
    ) -> Result<(), CodecError> {
        if message.kind != Kind::Frame {
            return Err(CodecError::Malformed(format!(
                "unexpected {:?} on the frame channel",
                message.kind
            )));
        }
        let meta: FrameMeta = decode_meta(&message.meta)?;
        // A frame is acknowledged even when it is discarded, so the helper
        // does not stall behind a message this side will not use.
        let ack = encode_meta(&Ack { seq: meta.seq })?;
        if meta.generation != self.generation {
            write_message(stream, Kind::Ack, &ack, &[], ACK_LIMITS)?;
            let mut state = lock(&self.state);
            state.stats.stale_frames += 1;
            state.spare = Some(message.body);
            return Ok(());
        }
        meta.check(message.body.len())
            .map_err(|error| CodecError::Malformed(error.message))?;
        write_message(stream, Kind::Ack, &ack, &[], ACK_LIMITS)?;
        let frame = Frame {
            tab: meta.tab,
            seq: meta.seq,
            width: meta.width,
            height: meta.height,
            bgra: message.body,
        };
        let mut state = lock(&self.state);
        state.stats.received_frames += 1;
        let Some((_, slot)) = state.tabs.get_mut(&frame.tab) else {
            state.stats.stale_frames += 1;
            state.spare = Some(frame.bgra);
            return Ok(());
        };
        if let Some(old) = slot.replace(frame) {
            state.stats.dropped_frames += 1;
            state.spare = Some(old.bgra);
        }
        drop(state);
        self.changed.notify_all();
        Ok(())
    }
}

struct ReaderCompletion {
    remaining: Mutex<usize>,
    changed: Condvar,
}

/// A reader loop for an external owner (the core starts it through Admission).
/// Running or dropping a job always publishes completion, including unwinding.
pub struct ReaderJob {
    inner: Arc<Inner>,
    stream: UnixStream,
    frames: bool,
    completion: Arc<ReaderCompletion>,
}
impl ReaderJob {
    pub fn run(self) {
        if self.frames {
            self.inner.frame_loop(&self.stream);
        } else {
            self.inner.control_loop(&self.stream);
        }
    }
}
impl Drop for ReaderJob {
    fn drop(&mut self) {
        *lock(&self.completion.remaining) -= 1;
        self.completion.changed.notify_all();
    }
}

pub struct RendererClient {
    inner: Arc<Inner>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    completion: Arc<ReaderCompletion>,
}

impl RendererClient {
    /// Constructs a connection without starting threads or waiting for Hello.
    /// The caller owns both jobs even if starting one fails. Dropping an
    /// unstarted job releases its completion; jobs never own the process.
    pub fn prepare(
        control: UnixStream,
        frames: UnixStream,
        config: ClientConfig,
    ) -> Result<(Self, [ReaderJob; 2]), ClientError> {
        let io = |error: std::io::Error| ClientError::Local(error.to_string());
        let writer = control.try_clone().map_err(io)?;
        writer
            .set_write_timeout(Some(config.io_timeout))
            .map_err(io)?;
        frames
            .set_write_timeout(Some(config.io_timeout))
            .map_err(io)?;
        let inner = Arc::new(Inner {
            generation: config.generation,
            writer: Mutex::new(writer),
            control_abort: control.try_clone().map_err(io)?,
            frames_abort: frames.try_clone().map_err(io)?,
            next_id: AtomicU64::new(1),
            shutdown_id: AtomicU64::new(0),
            shutdown_acknowledged: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            config,
        });
        let completion = Arc::new(ReaderCompletion {
            remaining: Mutex::new(2),
            changed: Condvar::new(),
        });
        let jobs = [
            ReaderJob {
                inner: inner.clone(),
                stream: control,
                frames: false,
                completion: completion.clone(),
            },
            ReaderJob {
                inner: inner.clone(),
                stream: frames,
                frames: true,
                completion: completion.clone(),
            },
        ];
        Ok((
            Self {
                inner,
                threads: Mutex::new(Vec::new()),
                completion,
            },
            jobs,
        ))
    }

    /// Standalone path: owns its threads and performs Hello as before.
    pub fn connect(
        control: UnixStream,
        frames: UnixStream,
        config: ClientConfig,
    ) -> Result<(Self, HelperInfo), ClientError> {
        let (client, jobs) = Self::prepare(control, frames, config)?;
        for job in jobs {
            match std::thread::Builder::new()
                .name("renderer-reader".into())
                .spawn(move || job.run())
            {
                Ok(thread) => lock(&client.threads).push(thread),
                Err(error) => {
                    client.close();
                    return Err(ClientError::Local(error.to_string()));
                }
            }
        }
        let info = client.hello()?;
        Ok((client, info))
    }

    /// Waits for Hello. Core calls this only after releasing spawn Admission.
    pub fn hello(&self) -> Result<HelperInfo, ClientError> {
        match self.request(
            Request::Hello {
                protocol: PROTOCOL_VERSION,
            },
            &[],
            self.inner.config.hello_timeout,
        ) {
            Ok((Reply::Hello { protocol, helper }, _)) if protocol == PROTOCOL_VERSION => {
                Ok(HelperInfo { protocol, helper })
            }
            Ok((other, _)) => {
                self.close();
                Err(ClientError::Local(format!(
                    "unexpected Hello reply {other:?}"
                )))
            }
            Err(error) => {
                self.close();
                Err(error)
            }
        }
    }

    /// Wait for externally owned reader jobs after closing the connection.
    /// It does not join a thread; the owning ledger retains those join handles.
    pub fn wait_readers(&self) {
        let mut remaining = lock(&self.completion.remaining);
        while *remaining != 0 {
            remaining = self
                .completion
                .changed
                .wait(remaining)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn generation(&self) -> RendererGeneration {
        self.inner.generation
    }

    /// Sends one request and waits at most `timeout` for its response.
    pub fn request(
        &self,
        request: Request,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(Reply, Vec<u8>), ClientError> {
        if let Some(reason) = self.inner.closed() {
            return Err(ClientError::Closed(reason));
        }
        // Reserve the tab slot before Open can produce its initial paint. The
        // slot is never copied into a second hub queue. Timeout is uncertain:
        // retain the reservation until close or connection invalidation.
        let opened = if let Request::OpenArtifact(open) = &request {
            let mut state = lock(&self.inner.state);
            if state.tabs.contains_key(&open.tab) {
                None
            } else {
                if state.tabs.len() >= crate::MAX_TABS {
                    return Err(ClientError::Remote(RemoteError::new(
                        crate::ErrorKind::Busy,
                        "tab frame slots exhausted",
                    )));
                }
                state.tabs.insert(open.tab, (open.session, None));
                Some(open.tab)
            }
        } else {
            None
        };
        let close_tab = match &request {
            Request::CloseTab { tab } => Some(*tab),
            _ => None,
        };
        let close_session = match &request {
            Request::CloseSession { session } => Some(*session),
            _ => None,
        };
        let id = RequestId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let is_shutdown = matches!(request, Request::Shutdown);
        let frame = RequestFrame {
            generation: self.inner.generation,
            id,
            request,
        };
        let meta = match encode_meta(&frame) {
            Ok(meta) => meta,
            Err(error) => {
                if let Some(tab) = opened {
                    lock(&self.inner.state).tabs.remove(&tab);
                    self.inner.changed.notify_all();
                }
                return Err(ClientError::Local(error.to_string()));
            }
        };
        let (reply_tx, reply_rx) = channel();
        lock(&self.inner.pending).insert(id, reply_tx);
        if is_shutdown {
            self.inner.shutdown_id.store(id.0, Ordering::Release);
        }
        let written = {
            let writer = lock(&self.inner.writer);
            write_message(&*writer, Kind::Request, &meta, body, CONTROL_LIMITS)
        };
        if let Err(error) = written {
            lock(&self.inner.pending).remove(&id);
            return Err(match error {
                // Nothing was sent; the channel is still usable.
                CodecError::TooLarge { .. } => {
                    if let Some(tab) = opened {
                        lock(&self.inner.state).tabs.remove(&tab);
                        self.inner.changed.notify_all();
                    }
                    ClientError::Local(error.to_string())
                }
                other => {
                    self.inner.peer_ended(other);
                    ClientError::Closed(self.inner.closed().unwrap_or(CloseReason::PeerClosed))
                }
            });
        }
        let result = match reply_rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(_) => {
                // Either the wait ran out or the connection ended; a response
                // that arrives later is counted and discarded.
                lock(&self.inner.pending).remove(&id);
                match self.inner.closed() {
                    Some(reason) => Err(ClientError::Closed(reason)),
                    None => Err(ClientError::Timeout),
                }
            }
        };
        let mut state = lock(&self.inner.state);
        if result.is_ok() {
            if let Some(tab) = close_tab {
                state.tabs.remove(&tab);
            }
            if let Some(session) = close_session {
                state.tabs.retain(|_, (s, _)| *s != session);
            }
        } else if matches!(result, Err(ClientError::Remote(_)))
            && let Some(tab) = opened
        {
            state.tabs.remove(&tab);
        }
        drop(state);
        self.inner.changed.notify_all();
        result
    }

    fn wait_for(&self, helper_timeout_ms: u32) -> Duration {
        Duration::from_millis(u64::from(helper_timeout_ms)) + self.inner.config.reply_grace
    }

    fn expect_done(&self, request: Request, timeout: Duration) -> Result<(), ClientError> {
        match self.request(request, &[], timeout)? {
            (Reply::Done, _) => Ok(()),
            (other, _) => Err(ClientError::Local(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn open_artifact(&self, open: OpenArtifact, body: &[u8]) -> Result<Viewport, ClientError> {
        let timeout = self.wait_for(open.timeout_ms);
        match self.request(Request::OpenArtifact(open), body, timeout)? {
            (Reply::Opened { viewport, .. }, _) => Ok(viewport),
            (other, _) => Err(ClientError::Local(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn resize(&self, tab: TabId, viewport: Viewport) -> Result<(), ClientError> {
        self.expect_done(
            Request::Resize { tab, viewport },
            self.inner.config.reply_grace,
        )
    }

    pub fn set_streaming(&self, tab: TabId, enabled: bool) -> Result<(), ClientError> {
        self.expect_done(
            Request::SetStreaming { tab, enabled },
            self.inner.config.reply_grace,
        )
    }

    pub fn set_frame_rate(&self, tab: TabId, frame_rate: u8) -> Result<(), ClientError> {
        self.expect_done(
            Request::SetFrameRate { tab, frame_rate },
            self.inner.config.reply_grace,
        )
    }

    /// A PNG of the loaded page.
    pub fn capture_png(&self, tab: TabId, timeout_ms: u32) -> Result<Vec<u8>, ClientError> {
        match self.request(
            Request::Capture { tab, timeout_ms },
            &[],
            self.wait_for(timeout_ms),
        )? {
            (Reply::Png, png) => Ok(png),
            (other, _) => Err(ClientError::Local(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn cdp(
        &self,
        tab: TabId,
        method: &str,
        params: Value,
        timeout_ms: u32,
    ) -> Result<Value, ClientError> {
        let request = Request::Cdp {
            tab,
            method: method.to_owned(),
            params,
            timeout_ms,
        };
        match self.request(request, &[], self.wait_for(timeout_ms))? {
            (Reply::Cdp { result }, _) => Ok(result),
            (other, _) => Err(ClientError::Local(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn settle(&self, tab: TabId, timeout_ms: u32) -> Result<(), ClientError> {
        self.expect_done(
            Request::Settle { tab, timeout_ms },
            self.wait_for(timeout_ms),
        )
    }

    pub fn take_notices(&self, tab: TabId) -> Result<Notices, ClientError> {
        match self.request(
            Request::TakeNotices { tab },
            &[],
            self.inner.config.reply_grace,
        )? {
            (Reply::Notices(notices), _) => Ok(notices),
            (other, _) => Err(ClientError::Local(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn close_tab(&self, tab: TabId) -> Result<(), ClientError> {
        self.expect_done(Request::CloseTab { tab }, self.inner.config.reply_grace)
    }

    pub fn close_session(&self, session: SessionId) -> Result<(), ClientError> {
        self.expect_done(
            Request::CloseSession { session },
            self.inner.config.reply_grace,
        )
    }

    /// Takes the newest frame, waiting up to `timeout` (`None`: until one
    /// arrives or the connection ends). `None` means no frame.
    pub fn wait_frame(&self, timeout: Option<Duration>) -> Option<Frame> {
        self.wait_frame_matching(None, timeout)
    }

    /// Take only this tab's latest frame without consuming other tabs.
    pub fn wait_frame_for(&self, tab: TabId, timeout: Option<Duration>) -> Option<Frame> {
        self.wait_frame_matching(Some(tab), timeout)
    }
    fn wait_frame_matching(&self, tab: Option<TabId>, timeout: Option<Duration>) -> Option<Frame> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut state = lock(&self.inner.state);
        loop {
            if let Some(frame) = state
                .tabs
                .iter_mut()
                .filter(|(id, _)| tab.is_none_or(|tab| **id == tab))
                .find_map(|(_, (_, slot))| slot.take())
            {
                return Some(frame);
            }
            if state.closed.is_some() || tab.is_some_and(|id| !state.tabs.contains_key(&id)) {
                return None;
            }
            state = match deadline {
                None => self
                    .inner
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    self.inner
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
    }

    /// Returns a consumed frame's buffer so the next frame reuses it.
    pub fn recycle_frame(&self, frame: Frame) {
        lock(&self.inner.state).spare = Some(frame.bgra);
    }

    /// The next event, waiting up to `timeout` (`None`: until one arrives or
    /// the connection ends).
    pub fn next_event(&self, timeout: Option<Duration>) -> Option<EventKind> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut state = lock(&self.inner.state);
        loop {
            if let Some(event) = state.events.pop_front() {
                return Some(event);
            }
            if state.closed.is_some() {
                return None;
            }
            state = match deadline {
                None => self
                    .inner
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    self.inner
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
    }

    pub fn stats(&self) -> ClientStats {
        lock(&self.inner.state).stats
    }

    pub fn closed(&self) -> Option<CloseReason> {
        self.inner.closed()
    }

    /// Waits until the connection has ended and says why.
    pub fn wait_closed(&self) -> CloseReason {
        let mut state = lock(&self.inner.state);
        loop {
            if let Some(reason) = &state.closed {
                return reason.clone();
            }
            state = self
                .inner
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Like [`wait_closed`](Self::wait_closed), but gives up after `timeout`
    /// and returns `None` while the connection is still open.
    pub fn wait_closed_for(&self, timeout: Duration) -> Option<CloseReason> {
        let state = lock(&self.inner.state);
        let (state, _) = self
            .inner
            .changed
            .wait_timeout_while(state, timeout, |state| state.closed.is_none())
            .unwrap_or_else(|e| e.into_inner());
        state.closed.clone()
    }

    /// Asks the helper to end and waits for the connection to close, both within
    /// `timeout` altogether. A helper that acknowledges but keeps the channels
    /// open is cut off at the deadline ([`CloseReason::ShutdownTimedOut`]); the
    /// returned reason is final either way.
    pub fn shutdown(&self, timeout: Duration) -> CloseReason {
        let deadline = std::time::Instant::now().checked_add(timeout);
        match self.request(Request::Shutdown, &[], timeout) {
            // The helper closes its channels right after answering.
            Ok(_) => {
                let budget = deadline
                    .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
                self.finish_shutdown(budget)
            }
            Err(_) => {
                self.close();
                self.closed().unwrap_or(CloseReason::LocalClose)
            }
        }
    }

    /// Waits for the acknowledged helper to close the connection, at most
    /// `budget` (unbounded for `None`). After that the connection is closed
    /// from here, so the result is always final.
    fn finish_shutdown(&self, budget: Option<Duration>) -> CloseReason {
        let reason = match budget {
            Some(budget) => self.wait_closed_for(budget),
            None => Some(self.wait_closed()),
        }
        .unwrap_or_else(|| {
            self.inner.close(CloseReason::ShutdownTimedOut);
            self.closed().unwrap_or(CloseReason::ShutdownTimedOut)
        });
        self.join_threads();
        reason
    }

    /// Ends the connection now. The helper sees its control channel close and
    /// stops by itself.
    pub fn close(&self) {
        self.inner.close(CloseReason::LocalClose);
        self.join_threads();
    }

    fn join_threads(&self) {
        let threads: Vec<_> = lock(&self.threads).drain(..).collect();
        for thread in threads {
            if thread.thread().id() != std::thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for RendererClient {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{CONTROL_LIMITS, Kind, encode_meta, read_message, write_message};
    use crate::protocol::{Outcome, RequestFrame, ResponseFrame};

    /// A connected client whose peer answers `Hello` and then keeps both
    /// channels open until the client closes them.
    fn client_with_a_silent_peer() -> (RendererClient, std::thread::JoinHandle<()>) {
        let generation = RendererGeneration(1);
        let (control, peer_control) = UnixStream::pair().unwrap();
        let (frames, peer_frames) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            let hello = read_message(
                &peer_control,
                CONTROL_LIMITS,
                Vec::new(),
                Duration::from_secs(3600),
            )
            .unwrap();
            let request: RequestFrame = serde_json::from_slice(&hello.meta).unwrap();
            let response = ResponseFrame {
                generation,
                id: request.id,
                outcome: Outcome::Ok {
                    reply: Reply::Hello {
                        protocol: PROTOCOL_VERSION,
                        helper: "silent".into(),
                    },
                },
            };
            write_message(
                &peer_control,
                Kind::Response,
                &encode_meta(&response).unwrap(),
                &[],
                CONTROL_LIMITS,
            )
            .unwrap();
            // Holds both channels until the client cuts them: reading ends
            // with the close, and nothing else is ever sent.
            let _ = read_message(
                &peer_control,
                CONTROL_LIMITS,
                Vec::new(),
                Duration::from_secs(3600),
            );
            drop(peer_frames);
        });
        let (client, _) = RendererClient::connect(control, frames, ClientConfig::new(generation))
            .expect("the peer answers Hello");
        (client, peer)
    }

    #[test]
    fn a_helper_that_acknowledges_but_never_closes_is_cut_off_at_the_deadline() {
        let (client, peer) = client_with_a_silent_peer();
        // No budget left: the connection is still open, so it is closed here.
        assert_eq!(client.wait_closed_for(Duration::ZERO), None);
        assert_eq!(
            client.finish_shutdown(Some(Duration::ZERO)),
            CloseReason::ShutdownTimedOut
        );
        assert_eq!(client.closed(), Some(CloseReason::ShutdownTimedOut));
        peer.join().unwrap();
    }

    #[test]
    fn a_frame_cut_short_by_a_close_leaves_the_decision_to_the_control_channel() {
        use std::io::Write;
        let (client, peer) = client_with_a_silent_peer();
        // A frame header announcing a body that never fully arrives, then the
        // close an ordered stop or a crash brings: the same event as a clean
        // close between frames, only mistimed.
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut header = b"ERIP".to_vec();
        header.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        header.extend_from_slice(&[Kind::Frame as u8, 0]);
        header.extend_from_slice(&2u32.to_le_bytes());
        header.extend_from_slice(&64u32.to_le_bytes());
        writer.write_all(&header).unwrap();
        writer.write_all(b"{}").unwrap();
        writer.write_all(&[0; 10]).unwrap();
        drop(writer);
        client.inner.frame_loop(&reader);
        assert_eq!(client.closed(), None, "only the control channel decides");
        client.inner.close(CloseReason::PeerClosed);
        peer.join().unwrap();
    }

    #[test]
    fn a_connection_that_closed_in_time_keeps_its_own_reason() {
        let (client, peer) = client_with_a_silent_peer();
        client.inner.close(CloseReason::PeerClosed);
        assert_eq!(
            client.finish_shutdown(Some(Duration::ZERO)),
            CloseReason::PeerClosed
        );
        peer.join().unwrap();
    }
}
