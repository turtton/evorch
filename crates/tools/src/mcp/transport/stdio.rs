//! Approval-gated stdio process supervision. No shell, inherited credentials or replay.
use super::api::*;
use crate::mcp::{McpError, McpErrorKind, types::*};
use async_trait::async_trait;
use sandbox::process::{GroupChild, apply_launch_policy};
use serde::Serialize;
use serde_json::json;
use std::{
    io,
    process::Stdio,
    sync::{
        Arc, Mutex as SyncMutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::ChildStdin,
    sync::{Mutex, oneshot, watch},
};

/// Exact allowlist from the launch contract; deliberately not prefix matching.
pub const INHERITED_ENVIRONMENT: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_ADDRESS",
    "LC_IDENTIFICATION",
    "LC_MEASUREMENT",
    "LC_NAME",
    "LC_PAPER",
    "LC_TELEPHONE",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_DIRS",
    "XDG_DATA_DIRS",
];
const MAX_DIAGNOSTIC_KEYS: usize = 16;
const MAX_DIAGNOSTIC_KEY_BYTES: usize = 64;

/// Production grace periods; tests may inject zero grace or advance Tokio time.
#[derive(Debug, Clone)]
pub struct StdioShutdownPolicy {
    pub stdin_grace: Duration,
    pub term_grace: Duration,
    pub clock: Arc<dyn StdioShutdownClock>,
}
impl Default for StdioShutdownPolicy {
    fn default() -> Self {
        Self {
            stdin_grace: Duration::from_secs(1),
            term_grace: Duration::from_secs(1),
            clock: Arc::new(TokioShutdownClock),
        }
    }
}
/// Injectable grace clock. Test clocks are event-driven; they never use wall
/// time to decide whether a subprocess is correct.
#[async_trait]
pub trait StdioShutdownClock: Send + Sync + std::fmt::Debug {
    async fn expired(&self, grace: Duration);
}
#[derive(Debug)]
struct TokioShutdownClock;
#[async_trait]
impl StdioShutdownClock for TokioShutdownClock {
    async fn expired(&self, grace: Duration) {
        let _ = tokio::time::timeout(grace, std::future::pending::<()>()).await;
    }
}
#[derive(Debug, Default)]
pub struct StdioTransportFactory {
    pub shutdown_policy: StdioShutdownPolicy,
}
#[async_trait]
impl McpTransportFactory for StdioTransportFactory {
    async fn connect(
        &self,
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        limiter: Arc<dyn McpProcessLimiter>,
        cancel: CancelContext,
    ) -> Result<Arc<dyn McpTransport>, McpError> {
        Ok(StdioTransport::connect_with_policy(
            server,
            permit,
            limiter,
            cancel,
            self.shutdown_policy.clone(),
        )
        .await?)
    }
}

/// One run/server owner. Restart never retries a request: callers must negotiate
/// the fresh process and compare its definitions with their frozen snapshot.
pub struct StdioTransport {
    server: Arc<ResolvedMcpServer>,
    permit: Arc<ApprovalPermit>,
    limiter: Arc<dyn McpProcessLimiter>,
    current: Mutex<Arc<Session>>,
    restarts: AtomicUsize,
    closed: AtomicBool,
    policy: StdioShutdownPolicy,
}
impl StdioTransport {
    pub async fn connect(
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        limiter: Arc<dyn McpProcessLimiter>,
        cancel: CancelContext,
    ) -> Result<Arc<Self>, McpError> {
        Self::connect_with_policy(
            server,
            permit,
            limiter,
            cancel,
            StdioShutdownPolicy::default(),
        )
        .await
    }
    pub async fn connect_with_policy(
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        limiter: Arc<dyn McpProcessLimiter>,
        cancel: CancelContext,
        policy: StdioShutdownPolicy,
    ) -> Result<Arc<Self>, McpError> {
        let server = Arc::new(server);
        let permit = Arc::new(permit);
        let session = spawn_session(&server, &permit, &*limiter, &cancel, policy.clone())?;
        Ok(Arc::new(Self {
            server,
            permit,
            limiter,
            current: Mutex::new(session),
            restarts: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            policy,
        }))
    }
    /// At most one explicit replacement for this run/server, after old reap.
    /// Re-approval and shared project admission are checked again before spawn.
    pub async fn restart(&self, cancel: CancelContext) -> Result<(), McpError> {
        let mut current = self.current.lock().await;
        if self.closed.load(Ordering::Acquire)
            || self.restarts.fetch_add(1, Ordering::AcqRel) >= MAX_RESTARTS_PER_RUN_SERVER
        {
            return Err(self.error(McpErrorKind::Unavailable));
        }
        current.stop.cancel();
        current.reaped().await.map_err(|k| self.error(k))?;
        *current = spawn_session(
            &self.server,
            &self.permit,
            &*self.limiter,
            &cancel,
            self.policy.clone(),
        )?;
        Ok(())
    }
    fn error(&self, kind: McpErrorKind) -> McpError {
        safe_error(&self.server, kind)
    }
    async fn session(&self) -> Result<Arc<Session>, McpError> {
        validate_approval(&self.server, &self.permit).map_err(|k| self.error(k))?;
        if self.closed.load(Ordering::Acquire) {
            return Err(self.error(McpErrorKind::Unavailable));
        }
        let session = self.current.lock().await.clone();
        if session.owner_cancel.is_cancelled() {
            return Err(self.error(McpErrorKind::Cancelled));
        }
        Ok(session)
    }
}
impl Drop for StdioTransport {
    fn drop(&mut self) {
        let session = self.current.get_mut().clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let pending = session.begin_cancel(None).await.0;
                session.stop.cancel();
                let _ = session.reaped().await;
                if let Some(pending) = pending {
                    let _ = pending.reply.send(Err(McpErrorKind::Cancelled));
                }
            });
        } else {
            session.stop.cancel();
        }
    }
}

struct Pending {
    context: CancelRequest,
    call: bool,
    reply: oneshot::Sender<Result<McpResponseEnvelope, McpErrorKind>>,
    frames: Vec<RpcFrame>,
    bytes: usize,
    count: usize,
}
struct Session {
    writer: Mutex<Option<ChildStdin>>,
    /// Only the exchange holds this lock. Cancellation never acquires it.
    call: Mutex<()>,
    cancellation: Mutex<()>,
    pending: SyncMutex<Option<Pending>>,
    legacy: AtomicBool,
    stop: CancelContext,
    done: watch::Receiver<Option<Result<(), McpErrorKind>>>,
    write_timeout: Duration,
    owner_cancel: CancelContext,
}
impl Session {
    fn pending(&self) -> MutexGuard<'_, Option<Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn fail(&self, kind: McpErrorKind) {
        if let Some(p) = self.pending().take() {
            let kind = if p.call
                && p.context.issued
                && matches!(
                    kind,
                    McpErrorKind::Transport | McpErrorKind::Unavailable | McpErrorKind::Timeout
                ) {
                McpErrorKind::OutcomeUnknown
            } else {
                kind
            };
            let _ = p.reply.send(Err(kind));
        }
    }
    async fn reaped(&self) -> Result<(), McpErrorKind> {
        let mut done = self.done.clone();
        loop {
            if let Some(result) = *done.borrow_and_update() {
                return result;
            }
            if done.changed().await.is_err() {
                return Err(McpErrorKind::Transport);
            }
        }
    }
    async fn write<T: Serialize + Sync>(&self, value: &T) -> Result<(), McpErrorKind> {
        self.write_frame(value, false).await
    }
    async fn write_frame<T: Serialize + Sync>(
        &self,
        value: &T,
        request: bool,
    ) -> Result<(), McpErrorKind> {
        let mut bytes = encode_bounded(value)?;
        // Public transport callers must not put malformed JSON-RPC on stdin.
        crate::mcp::codec::parse_frame(&bytes)?;
        bytes.push(b'\n');
        let write = async {
            let mut writer = self.writer.lock().await;
            let writer = writer.as_mut().ok_or(McpErrorKind::Transport)?;
            if request {
                let mut pending = self.pending();
                let p = pending.as_mut().ok_or(McpErrorKind::Cancelled)?;
                p.context.issued = true;
            }
            writer
                .write_all(&bytes)
                .await
                .map_err(|_| McpErrorKind::Transport)?;
            writer.flush().await.map_err(|_| McpErrorKind::Transport)
        };
        tokio::select! {
            biased;
            () = self.stop.cancelled() => Err(McpErrorKind::Transport),
            result = tokio::time::timeout(self.write_timeout, write) => result.unwrap_or(Err(McpErrorKind::Timeout)),
        }
    }
    fn abandon_probe(&self, id: &RpcId) {
        let mut pending = self.pending();
        if pending.as_ref().is_some_and(|p| &p.context.id == id)
            && let Some(p) = pending.take()
        {
            let _ = p.reply.send(Err(McpErrorKind::Timeout));
        }
    }
    async fn begin_cancel(
        &self,
        request: Option<CancelRequest>,
    ) -> (Option<Pending>, Result<(), McpErrorKind>) {
        let _cancellation = self.cancellation.lock().await;
        // Local bookkeeping is authoritative: never notify completed, unissued,
        // unknown IDs or a caller-supplied forged in-flight status.
        let pending = {
            let mut slot = self.pending();
            if slot
                .as_ref()
                .is_some_and(|p| request.as_ref().is_none_or(|r| p.context.id == r.id))
            {
                slot.take()
            } else {
                None
            }
        };
        let Some(pending) = pending else {
            return (None, Ok(()));
        };
        let action = cancellation_action(&pending.context);
        let result = if action == CancellationAction::NotifyAndFlushThenClose {
            self.write(&RpcNotification {
                jsonrpc: "2.0".into(),
                method: "notifications/cancelled".into(),
                params: Some(json!({"requestId": pending.context.id})),
            })
            .await
        } else {
            Ok(())
        };
        self.stop.cancel();
        if let Err(kind) = result {
            tracing::warn!(class = %kind, "MCP stdio cancellation flush failed; cleanup continues");
        }
        (Some(pending), result)
    }
    async fn cancel_request(&self, request: CancelRequest) -> Result<(), McpErrorKind> {
        let (pending, result) = self.begin_cancel(Some(request)).await;
        let Some(pending) = pending else {
            return result;
        };
        // Broken pipe/flush failure cannot short-circuit cleanup.
        let reaped = self.reaped().await;
        let _ = pending.reply.send(Err(McpErrorKind::Cancelled));
        result.and(reaped)
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn exchange(
        &self,
        request: RpcRequest,
        cancel: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError> {
        let session = self.session().await?;
        let _call = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(self.error(McpErrorKind::Cancelled)),
            () = session.owner_cancel.cancelled() => return Err(self.error(McpErrorKind::Cancelled)),
            lock = session.call.lock() => lock,
        };
        if session.stop.is_cancelled() {
            return Err(self.error(McpErrorKind::Unavailable));
        }
        let legacy = request.method == "initialize"
            || (session.legacy.load(Ordering::Acquire) && request.method != "server/discover");
        session.legacy.store(legacy, Ordering::Release);
        let context = CancelRequest {
            id: request.id.clone(),
            era: if legacy {
                ProtocolEra::Legacy
            } else {
                ProtocolEra::Modern
            },
            transport: TransportKind::Stdio,
            phase: if request.method == "initialize" {
                RequestPhase::Initialize
            } else {
                RequestPhase::Request
            },
            issued: false,
            in_flight: true,
        };
        let (reply, response) = oneshot::channel();
        let call = request.method == "tools/call";
        let probe = request.method == "server/discover";
        *session.pending() = Some(Pending {
            context: context.clone(),
            call,
            reply,
            frames: Vec::new(),
            bytes: 0,
            count: 0,
        });
        let mut guard = ExchangeGuard {
            session: session.clone(),
            context: Some(context.clone()),
            probe,
            request_cancel: cancel.clone(),
        };
        let timeout = if matches!(request.method.as_str(), "initialize" | "server/discover") {
            self.server.startup_timeout
        } else {
            self.server.request_timeout
        };
        let exchange = async {
            session.write_frame(&request, true).await?;
            response.await.unwrap_or(Err(McpErrorKind::Transport))
        };
        let result = tokio::select! {
            biased;
            result = tokio::time::timeout(timeout, exchange) => match result {
                Ok(result) => result,
                Err(_) => {
                    if probe { session.abandon_probe(&context.id); }
                    else { let _ = session.cancel_request(context.clone()).await; }
                    Err(McpErrorKind::Timeout)
                }
            },
            () = cancel.cancelled() => {
                let _ = session.cancel_request(context.clone()).await;
                Err(McpErrorKind::Cancelled)
            }
        };
        guard.context = None;
        if probe && matches!(result, Err(McpErrorKind::Timeout)) {
            session.abandon_probe(&context.id);
            return Err(self.error(McpErrorKind::Timeout));
        }
        if let Err(kind) = result {
            // Also stops a process following protocol, framing or write failures.
            session.fail(kind);
            session.stop.cancel();
            let _ = session.reaped().await;
            let kind = if call && matches!(kind, McpErrorKind::Transport | McpErrorKind::Timeout) {
                McpErrorKind::OutcomeUnknown
            } else {
                kind
            };
            return Err(self.error(kind));
        }
        result.map_err(|k| self.error(k))
    }
    async fn notify(&self, notification: RpcNotification) -> Result<(), McpError> {
        self.session()
            .await?
            .write(&notification)
            .await
            .map_err(|k| self.error(k))
    }
    async fn respond(&self, response: RpcFrame) -> Result<(), McpError> {
        let session = self.session().await?;
        if !session.legacy.load(Ordering::Acquire) || !matches!(response, RpcFrame::Error { .. }) {
            return Err(self.error(McpErrorKind::Protocol));
        }
        session.write(&response).await.map_err(|k| self.error(k))
    }
    async fn cancel(&self, request: CancelRequest) -> Result<(), McpError> {
        // Do not revalidate approval here: revocation must still permit cleanup.
        let session = self.current.lock().await.clone();
        session
            .cancel_request(request)
            .await
            .map_err(|k| self.error(k))
    }
    async fn shutdown(&self, _reason: ShutdownReason) -> Result<(), McpError> {
        self.closed.store(true, Ordering::Release);
        let session = self.current.lock().await.clone();
        let pending = session.begin_cancel(None).await.0;
        session.stop.cancel();
        let result = session.reaped().await;
        if let Some(pending) = pending {
            let _ = pending.reply.send(Err(McpErrorKind::Cancelled));
        }
        result.map_err(|k| self.error(k))
    }
}
struct ExchangeGuard {
    session: Arc<Session>,
    context: Option<CancelRequest>,
    probe: bool,
    request_cancel: CancelContext,
}
impl Drop for ExchangeGuard {
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            if self.probe
                && !self.request_cancel.is_cancelled()
                && !self.session.owner_cancel.is_cancelled()
            {
                // Legacy peers may ignore the modern-era probe. Its caller's
                // deadline is not run cancellation: allow initialize on this
                // process and discard the late, never-reused probe id.
                self.session.abandon_probe(&context.id);
                return;
            }
            let session = self.session.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = session.cancel_request(context).await;
                });
            } else {
                session.stop.cancel();
            }
        }
    }
}
fn safe_error(server: &ResolvedMcpServer, kind: McpErrorKind) -> McpError {
    McpError {
        server: server.scope.server_id.clone(),
        method: "stdio",
        request_id: None,
        kind,
    }
}
fn validate_approval(
    server: &ResolvedMcpServer,
    permit: &ApprovalPermit,
) -> Result<(), McpErrorKind> {
    if permit.is_revoked() {
        return Err(McpErrorKind::ApprovalRevoked);
    }
    if permit.scope() != &server.scope || permit.fingerprint() != server.fingerprint {
        return Err(McpErrorKind::NeedsApproval);
    }
    Ok(())
}
fn spawn_session(
    server: &ResolvedMcpServer,
    approval: &ApprovalPermit,
    limiter: &dyn McpProcessLimiter,
    cancel: &CancelContext,
    policy: StdioShutdownPolicy,
) -> Result<Arc<Session>, McpError> {
    let error = |k| safe_error(server, k);
    validate_approval(server, approval).map_err(error)?;
    if cancel.is_cancelled() {
        return Err(error(McpErrorKind::Cancelled));
    }
    let ResolvedMcpTransport::Stdio {
        executable,
        args,
        cwd,
        env,
    } = &server.transport
    else {
        return Err(error(McpErrorKind::Configuration));
    };
    if !executable.is_absolute() || !cwd.is_absolute() {
        return Err(error(McpErrorKind::Configuration));
    }
    let permit = limiter
        .try_acquire(&server.scope.canonical_root)
        .map_err(error)?;
    let mut command = std::process::Command::new(executable);
    command
        .args(args.iter().map(sandbox::Secret::expose))
        .current_dir(cwd)
        .env_clear();
    for key in INHERITED_ENVIRONMENT {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in env {
        command.env(key, value.expose());
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_launch_policy(&mut command);
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    // Admission may have raced with revocation/cancellation; check once more at spawn.
    validate_approval(server, approval).map_err(error)?;
    if cancel.is_cancelled() {
        return Err(error(McpErrorKind::Cancelled));
    }
    let child = GroupChild::spawn(&mut command).map_err(|_| {
        // Never emit the OS error: command paths or env values can be secrets.
        let keys: Vec<String> = std::env::vars_os().filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| !INHERITED_ENVIRONMENT.contains(&key.as_str()) && !env.contains_key(key))
            .filter(|key| key.len() <= MAX_DIAGNOSTIC_KEY_BYTES && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            .take(MAX_DIAGNOSTIC_KEYS).collect();
        tracing::warn!(not_inherited_environment_keys = ?keys,
            "MCP stdio launch failed; only the documented environment allowlist and explicit env are passed; listed keys are possible context, not a diagnosed cause");
        error(McpErrorKind::Transport)
    })?;
    let mut owner = ChildLease {
        child: Some(child),
        permit: Some(permit),
    };
    let child = owner.child.as_mut().expect("owned child");
    let stdin = child.take_stdin();
    let stdout = child.take_stdout().expect("configured stdout pipe");
    let stderr = child.take_stderr().expect("configured stderr pipe");
    let (done, completion) = watch::channel(None);
    let session = Arc::new(Session {
        writer: Mutex::new(stdin),
        call: Mutex::new(()),
        cancellation: Mutex::new(()),
        pending: SyncMutex::new(None),
        legacy: AtomicBool::new(false),
        stop: CancelContext::default(),
        done: completion,
        write_timeout: server.request_timeout,
        owner_cancel: cancel.clone(),
    });
    let reader_session = session.clone();
    let reader = tokio::spawn(async move { read_stdout(stdout, &reader_session).await });
    // Discard stderr in fixed chunks, without retaining or tracing any raw bytes.
    let drainer = tokio::spawn(async move { drain_stderr(stderr).await });
    let supervisor_session = session.clone();
    tokio::spawn(async move {
        supervise(owner, supervisor_session, reader, drainer, done, policy).await;
    });
    Ok(session)
}
/// Unwinding/runtime teardown fallback: retain the admission permit until an OS
/// wait has completed, even when no async executor remains to run cleanup.
struct ChildLease {
    child: Option<GroupChild>,
    permit: Option<Box<dyn McpProcessPermit>>,
}
impl Drop for ChildLease {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill();
            let permit = self.permit.take();
            std::thread::spawn(move || {
                let _ = child.kill_and_reap_blocking();
                drop(child);
                drop(permit);
            });
        }
    }
}
async fn supervise(
    mut owner: ChildLease,
    session: Arc<Session>,
    mut reader: tokio::task::JoinHandle<Result<(), McpErrorKind>>,
    drainer: tokio::task::JoinHandle<io::Result<()>>,
    done: watch::Sender<Option<Result<(), McpErrorKind>>>,
    policy: StdioShutdownPolicy,
) {
    let child = owner.child.as_mut().expect("live child");
    let mut reader_finished = false;
    let mut cancelled_pending = None;
    let failure = tokio::select! {
        () = session.owner_cancel.cancelled() => {
            cancelled_pending = session.begin_cancel(None).await.0;
            McpErrorKind::Cancelled
        },
        () = session.stop.cancelled() => McpErrorKind::Transport,
        result = &mut reader => { reader_finished = true; result.unwrap_or(Err(McpErrorKind::Transport)).err().unwrap_or(McpErrorKind::Transport) },
        _ = child.exited() => McpErrorKind::Transport,
    };
    session.stop.cancel();
    // Cancels blocked writes and closes stdin before escalation.
    session.writer.lock().await.take();
    let exited = tokio::select! {
        result = child.exited() => result.is_ok(),
        () = policy.clock.expired(policy.stdin_grace) => false,
    };
    if !exited {
        tracing::debug!("MCP stdio stdin grace expired; sending SIGTERM");
        child.group().terminate();
        let exited = tokio::select! {
            result = child.exited() => result.is_ok(),
            () = policy.clock.expired(policy.term_grace) => false,
        };
        if !exited {
            tracing::warn!("MCP stdio termination grace expired; sending SIGKILL");
            child.kill();
        }
    }
    let result = child
        .wait()
        .await
        .map(|_| ())
        .map_err(|_| McpErrorKind::Transport);
    // Reaping kills group descendants too, releasing all inherited pipe writers.
    if !reader_finished {
        let _ = reader.await;
    }
    let _ = drainer.await;
    session.fail(failure);
    if result.is_ok() {
        owner.child.take();
        owner.permit.take();
    }
    // Drop fallback retains the permit on wait failure too.
    drop(owner);
    done.send_replace(Some(result));
    if let Some(pending) = cancelled_pending {
        let _ = pending.reply.send(Err(McpErrorKind::Cancelled));
    }
}
async fn drain_stderr(mut stderr: impl AsyncRead + Unpin) -> io::Result<()> {
    let mut bytes = [0u8; 8192];
    while stderr.read(&mut bytes).await? != 0 {}
    Ok(())
}
async fn read_stdout(
    stdout: impl AsyncRead + Unpin,
    session: &Session,
) -> Result<(), McpErrorKind> {
    let mut input = BufReader::new(stdout);
    let mut idle_frames = 0usize;
    loop {
        let Some(bytes) = read_line(&mut input).await? else {
            return Ok(());
        };
        let frame = crate::mcp::codec::parse_frame(&bytes)?;
        // Count even discarded unknown IDs, preventing unbounded per-call work.
        {
            let mut pending = session.pending();
            if let Some(p) = pending.as_mut() {
                p.count += 1;
                p.bytes = p.bytes.saturating_add(bytes.len());
                if p.count > MAX_FRAMES_PER_EXCHANGE || p.bytes > MAX_REQUEST_BYTES {
                    return Err(McpErrorKind::LimitExceeded);
                }
                idle_frames = 0;
            } else {
                idle_frames += 1;
                if idle_frames > MAX_FRAMES_PER_EXCHANGE {
                    return Err(McpErrorKind::LimitExceeded);
                }
            }
        }
        if let RpcFrame::Request(request) = &frame {
            if !session.legacy.load(Ordering::Acquire) {
                return Err(McpErrorKind::Protocol);
            }
            session
                .write(&RpcFrame::Error {
                    jsonrpc: "2.0".into(),
                    id: Some(request.id.clone()),
                    error: RpcError {
                        code: -32601,
                        message: "Method not found".into(),
                        data: None,
                    },
                })
                .await?;
            continue;
        }
        let id = match &frame {
            RpcFrame::Result { id, .. } => Some(id),
            RpcFrame::Error { id, .. } => id.as_ref(),
            _ => None,
        };
        let mut slot = session.pending();
        let Some(p) = slot.as_mut() else {
            continue;
        };
        if let Some(id) = id {
            if id != &p.context.id {
                continue;
            }
            p.frames.push(frame);
            let p = slot.take().expect("matching pending request");
            let envelope =
                McpResponseEnvelope::new(None, McpResponseHeaders::default(), p.frames, p.bytes);
            let _ = p.reply.send(envelope);
        } else if matches!(frame, RpcFrame::Notification(_)) {
            p.frames.push(frame);
        }
    }
}
/// Bound accumulation before copying. EOF without a newline is not a frame.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    input: &mut R,
) -> Result<Option<Vec<u8>>, McpErrorKind> {
    let mut line = Vec::new();
    loop {
        let available = input
            .fill_buf()
            .await
            .map_err(|_| McpErrorKind::Transport)?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(McpErrorKind::Protocol)
            };
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let len = newline.unwrap_or(available.len());
        if line.len().saturating_add(len) > MAX_FRAME_BYTES {
            return Err(McpErrorKind::LimitExceeded);
        }
        line.extend_from_slice(&available[..len]);
        input.consume(len + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

fn encode_bounded<T: Serialize>(value: &T) -> Result<Vec<u8>, McpErrorKind> {
    struct Buffer {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > MAX_REQUEST_BYTES {
                self.exceeded = true;
                return Err(io::Error::other("MCP request limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Buffer {
        bytes: Vec::new(),
        exceeded: false,
    };
    match serde_json::to_writer(&mut output, value) {
        Ok(()) => Ok(output.bytes),
        Err(_) if output.exceeded => Err(McpErrorKind::LimitExceeded),
        Err(_) => Err(McpErrorKind::Protocol),
    }
}
