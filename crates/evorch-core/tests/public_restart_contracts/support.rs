//! A real core Unix ingress (`unix::connection` behind a real `UnixListener`)
//! in front of the common `AdminService`, with a fake facade and an audit port
//! that can hold an ACK. Nothing here is a product seam: the fakes only stand
//! in for the leaves and the storage writer the core does not yet register.
#![allow(dead_code)]

use evorch_core::{backend::HostBackend, unix};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use storage::{StorageError, admin_audit::*};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{mpsc, watch},
};
use workbench_host::{
    CommandHost, admin::*, areas::tasks::AreaTasks, bootstrap::services::BackgroundServices,
    commands::CommandService,
};
use workbench_protocol::{
    commands::{Command, CommandResult},
    envelope::{
        ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope, ServerFrame,
    },
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    framing::encode_frame,
    handshake::{
        BuildInfo, ClientHello, HandshakeReply, LocalSurface, ProtocolVersion, ServerHello,
    },
    ids::{CapabilityName, ClientId, RequestId, SessionId},
    namespaces::lifecycle::{restart::*, *},
    queries::{Query, QueryReply},
    wire::{InstanceEpoch, Revision},
};

pub const MAX_FRAME: u32 = 1024 * 1024;

/// Bounds a wait that a regression could leave open forever; a test never
/// decides success or failure by this elapsing.
const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn guard<T>(wait: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_GUARD, wait)
        .await
        .expect("hang guard: the awaited event never arrived")
}

pub fn id(s: &str) -> AdminId {
    AdminId::new(s).unwrap()
}

fn fail(code: ErrorCode) -> WorkbenchError {
    WorkbenchError::new(code, "contract failure", RetryAdvice::Never)
}

pub fn status() -> AdminView {
    LifecycleReply::Restart(RestartReply::Status(RestartManagementStatus {
        audit: AdminAuditDiagnostics {
            denial_persistence_failures: Revision(999),
            denial_loss: None,
        },
        core_epoch: InstanceEpoch(999),
        status: RestartStatus {
            restart_id: None,
            phase: RestartPhase::Running,
            revision: 1,
            blockers: vec![],
            blocker_total: 0,
            blocker_omitted: 0,
            active_producers: 0,
        },
    }))
}

/// Counts every call that reaches the leaf, so a rejected request can prove it
/// never got that far. Effects only happen in a registered continuation.
pub struct Facade {
    pub calls: AtomicUsize,
    pub effects: Arc<AtomicUsize>,
    events: mpsc::UnboundedSender<(&'static str, String)>,
}

impl AdminFacade for Facade {
    fn inspect<'a>(
        &'a self,
        intent: &'a AdminIntent,
        _: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminInspection> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let inspection = match intent {
            LifecycleCommand::Restart(RestartCommand::Request(r)) => {
                AdminInspection::Restart(r.binding.clone())
            }
            other => panic!("the contract only inspects restart requests: {other:?}"),
        };
        Box::pin(async move { Ok(inspection) })
    }

    fn query<'a>(&'a self, query: &'a AdminQuery) -> AdminIo<'a, AdminView> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let view = match query {
            LifecycleQuery::OwnedGraphs(_) => LifecycleReply::OwnedGraphs(
                owned_graphs::OwnedGraphsReply::Undisposed(owned_graphs::UndisposedGraphs {
                    count: Revision(0),
                    references: vec![],
                    snapshot_revision: Revision(1),
                    truncated: false,
                    over_limit_count: Revision(0),
                    incomplete_disposals: owned_graphs::IncompleteDisposals::empty(),
                }),
            ),
            LifecycleQuery::Restart(RestartQuery::Targets) => {
                LifecycleReply::Restart(RestartReply::Targets(RestartTargets {
                    core_epoch: InstanceEpoch(999),
                    revision: Revision(1),
                    targets: vec![RestartTarget {
                        id: id("current"),
                        revision: Revision(2),
                    }],
                    truncated: false,
                }))
            }
            _ => status(),
        };
        Box::pin(async move { Ok(view) })
    }

    fn status(&self, _: &VerifiedConnection) -> Result<AdminView, AdminFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(status())
    }

    fn cancel(&self, intent: &AdminIntent) -> Result<AdminOutcome, AdminFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = self
            .events
            .send(("cancel", intent.operation_id().as_str().to_owned()));
        Ok(AdminOutcome::Cancelled)
    }

    fn commit(
        &self,
        intent: AdminIntent,
        expected: AdminInspection,
        admission: CommitAdmission,
        _connection: &VerifiedConnection,
    ) -> Result<AdminAccepted, AdminFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !expected.matches(&intent) {
            return Err(fail(ErrorCode::Conflict));
        }
        let operation = intent.operation_id().as_str().to_owned();
        let events = self.events.clone();
        let effects = self.effects.clone();
        Ok(admission.register(Box::pin(async move {
            // The effect happens strictly after commit, in the owned continuation.
            effects.fetch_add(1, Ordering::SeqCst);
            let _ = events.send(("effect", operation));
            Ok(AdminExecutionOutcome::Complete(AdminOutcome::Succeeded))
        })))
    }
}

type Held = (AuditReplyCallback, Result<AuditReply, StorageError>);

struct Memory {
    rows: BTreeMap<String, AuditRecord>,
    held: VecDeque<Held>,
}

/// In-memory audit writer. It can park the ACK of one operation's `Accept`
/// until the test releases it; every other write is answered at once.
pub struct Port {
    memory: Mutex<Memory>,
    hold: Mutex<Option<String>>,
    writes: AtomicUsize,
    events: mpsc::UnboundedSender<(&'static str, String)>,
}

impl Port {
    pub fn hold_accept_of(&self, operation_id: &str) {
        *self.hold.lock().unwrap() = Some(operation_id.to_owned());
    }

    pub fn release(&self) {
        *self.hold.lock().unwrap() = None;
        let held = std::mem::take(&mut self.memory.lock().unwrap().held);
        for (reply, result) in held {
            reply(result);
        }
    }

    pub fn record(&self, operation_id: &str) -> Option<AuditRecord> {
        self.memory.lock().unwrap().rows.get(operation_id).cloned()
    }

    pub fn row_count(&self) -> usize {
        self.memory.lock().unwrap().rows.len()
    }

    /// Journal writes the core submitted (accept, start, finalize, bind). The
    /// periodic denial flush and cleanup are background upkeep and not counted.
    pub fn write_count(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
}

impl AuditPort for Port {
    fn submit(&self, write: AuditWrite, reply: AuditReplyCallback) -> Result<(), StorageError> {
        if matches!(
            write,
            AuditWrite::Accept(_)
                | AuditWrite::Start { .. }
                | AuditWrite::Finalize { .. }
                | AuditWrite::BindRestart { .. }
        ) {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        let mut m = self.memory.lock().unwrap();
        let mut hold = false;
        let result = match write {
            AuditWrite::Accept(a) => {
                let operation = a.key.operation_id.clone();
                hold = self.hold.lock().unwrap().as_deref() == Some(operation.as_str());
                let _ = self.events.send(("accept", operation.clone()));
                if let Some(old) = m.rows.get(&operation) {
                    Ok(AuditReply::Reservation(AuditReservation::Existing(
                        old.clone(),
                    )))
                } else {
                    let row = AuditRecord {
                        accepted: a,
                        state: AuditState::Prepared,
                        restart_id: None,
                        outcome: None,
                        finished_at_ms: None,
                    };
                    m.rows.insert(operation, row.clone());
                    Ok(AuditReply::Reservation(AuditReservation::Created(row)))
                }
            }
            AuditWrite::Start { key } => {
                let _ = self.events.send(("start", key.operation_id.clone()));
                let r = m.rows.get_mut(&key.operation_id).unwrap();
                assert_eq!(r.accepted.key, key);
                r.state = AuditState::Started;
                Ok(AuditReply::Applied(r.clone()))
            }
            AuditWrite::Read { operation_id } => {
                Ok(AuditReply::Record(m.rows.get(&operation_id).cloned()))
            }
            AuditWrite::Finalize {
                key,
                outcome,
                at_ms,
            } => {
                let r = m.rows.get_mut(&key.operation_id).unwrap();
                assert_eq!(r.accepted.key, key);
                r.state = AuditState::Finished;
                r.outcome = Some(outcome);
                r.finished_at_ms = Some(at_ms);
                let applied = AuditReply::Applied(r.clone());
                let _ = self.events.send(("finalize", key.operation_id));
                Ok(applied)
            }
            AuditWrite::BindRestart { key, restart_id } => {
                let r = m.rows.get_mut(&key.operation_id).unwrap();
                r.restart_id = Some(restart_id);
                Ok(AuditReply::Applied(r.clone()))
            }
            AuditWrite::Denials { rows, .. } => Ok(AuditReply::Denials {
                stored: rows.len(),
                dropped: 0,
            }),
            AuditWrite::Cleanup { .. } | AuditWrite::Reconcile { .. } => {
                Ok(AuditReply::Cleanup(AuditCleanup {
                    accepted_removed: 0,
                    denials_removed: 0,
                    reconciled: 0,
                    more: false,
                }))
            }
            AuditWrite::Stats => Err(StorageError::Io("stats are not used here".into())),
        };
        if hold {
            m.held.push_back((reply, result));
        } else {
            drop(m);
            reply(result);
        }
        Ok(())
    }
}

/// A core whose Unix socket is served by the real `unix::connection`.
pub struct Core {
    _dir: test_socket_dir::SplitDir,
    pub socket: PathBuf,
    pub backend: Arc<HostBackend>,
    pub host: Arc<Mutex<CommandHost>>,
    pub facade: Arc<Facade>,
    pub port: Arc<Port>,
    pub epoch: InstanceEpoch,
    events: mpsc::UnboundedReceiver<(&'static str, String)>,
    seen: Vec<(&'static str, String)>,
    /// One value per finished connection task: whether it ended without error.
    ended: mpsc::UnboundedReceiver<bool>,
    stop: watch::Sender<bool>,
    server: tokio::task::JoinHandle<()>,
    tasks: AreaTasks,
    services: Option<BackgroundServices>,
}

/// A model that never answers: the host's runtime needs one and no chat starts.
struct Silent;

#[async_trait::async_trait]
impl runtime::AgentModel for Silent {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "silent".into()
    }
}

impl Core {
    /// `install_leaves = false` is the product state of this PR: the common
    /// service exists but no leaf facade is registered.
    pub async fn start(install_leaves: bool) -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let bus = Arc::new(event_bus::EventBus::new(64));
        let runtime = runtime::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(Silent),
        );
        let command_host = CommandHost::new(CommandService::new(
            runtime,
            tokio::runtime::Handle::current(),
        ));
        let epoch = command_host.projection().instance_epoch();
        let (events_tx, events) = mpsc::unbounded_channel();
        let port = Arc::new(Port {
            memory: Mutex::new(Memory {
                rows: BTreeMap::new(),
                held: VecDeque::new(),
            }),
            hold: Mutex::new(None),
            writes: AtomicUsize::new(0),
            events: events_tx.clone(),
        });
        let tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let admin = AdminService::new(
            AdminAudit::new(port.clone(), command_host.service().restart_gate()),
            tasks.clone(),
            epoch,
        );
        let facade = Arc::new(Facade {
            calls: AtomicUsize::new(0),
            effects: Arc::new(AtomicUsize::new(0)),
            events: events_tx,
        });
        if install_leaves {
            admin.install_restart(facade.clone());
            admin.install_quarantine(facade.clone());
            admin.install_owned_graphs(facade.clone());
        }
        let mut services = BackgroundServices::default();
        admin.register_worker(&mut services);
        let host = Arc::new(Mutex::new(command_host.with_admin(admin)));
        let backend = Arc::new(HostBackend::new(host.clone(), "contracts".into()));
        let socket = dir.sockets().join("contracts.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (stop, stopped) = watch::channel(false);
        let (done, ended) = mpsc::unbounded_channel();
        let server = tokio::spawn({
            let backend = backend.clone();
            async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (backend, stopped, done) = (backend.clone(), stopped.clone(), done.clone());
                    tokio::spawn(async move {
                        let result = unix::connection(stream, backend, stopped).await;
                        let _ = done.send(result.is_ok());
                    });
                }
            }
        });
        Self {
            _dir: dir,
            socket,
            backend,
            host,
            facade,
            port,
            epoch,
            events,
            seen: Vec::new(),
            ended,
            stop,
            server,
            tasks,
            services: Some(services),
        }
    }

    /// Wait for the named audit/facade event of `operation_id`. Events that
    /// arrive meanwhile are kept, so waits may be issued in any order.
    pub async fn event(&mut self, kind: &str, operation_id: &str) {
        let matches = |(k, op): &(&'static str, String)| *k == kind && op == operation_id;
        if let Some(at) = self.seen.iter().position(matches) {
            self.seen.remove(at);
            return;
        }
        loop {
            let event = guard(self.events.recv())
                .await
                .expect("event channel closed");
            if matches(&event) {
                return;
            }
            self.seen.push(event);
        }
    }

    /// Whether the next connection task to finish ended without error.
    pub async fn next_ended(&mut self) -> bool {
        guard(self.ended.recv())
            .await
            .expect("ended channel closed")
    }

    pub async fn connect(&self, hello: &ClientHello) -> Conn {
        Conn::open(&self.socket, hello)
            .await
            .expect("handshake accepted")
    }

    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        self.server.abort();
        self.tasks.request_stop();
        self.tasks.shutdown().await.unwrap();
        if let Some(mut services) = self.services.take() {
            services.shutdown().await.unwrap();
        }
    }
}

pub fn client_hello(name: &str, surface: Option<LocalSurface>) -> ClientHello {
    ClientHello {
        supported_versions: vec![ProtocolVersion::CURRENT],
        required_capabilities: vec![
            CapabilityName::new("commands.v1").unwrap(),
            CapabilityName::new("queries.v1").unwrap(),
        ],
        optional_capabilities: Vec::new(),
        client_build: BuildInfo {
            name: "contracts".into(),
            version: "1".into(),
            revision: None,
        },
        client_instance_id: ClientId::new(name).unwrap(),
        desired_core: None,
        local_surface: surface,
    }
}

pub async fn write_value(stream: &mut UnixStream, value: &impl serde::Serialize) {
    let bytes = encode_frame(value, MAX_FRAME).unwrap();
    stream.write_all(&bytes).await.unwrap();
}

/// `None` when the core closed the stream before a whole frame arrived.
pub async fn read_frame(stream: &mut UnixStream) -> Option<Vec<u8>> {
    let len = stream.read_u32().await.ok()?;
    assert!(len <= MAX_FRAME);
    let mut bytes = vec![0; len as usize];
    stream.read_exact(&mut bytes).await.ok()?;
    Some(bytes)
}

/// One raw connection: the test chooses what to send and in which order.
pub struct Conn {
    pub stream: UnixStream,
    pub hello: ServerHello,
}

impl Conn {
    pub async fn open(
        socket: &std::path::Path,
        hello: &impl serde::Serialize,
    ) -> Result<Self, Box<HandshakeReply>> {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        write_value(&mut stream, hello).await;
        let bytes = read_frame(&mut stream).await.expect("handshake reply");
        match serde_json::from_slice(&bytes).unwrap() {
            HandshakeReply::Accepted(hello) => Ok(Self { stream, hello }),
            rejected => Err(Box::new(rejected)),
        }
    }

    pub async fn send(&mut self, request: &RequestEnvelope) {
        write_value(&mut self.stream, request).await;
    }

    pub async fn send_raw(&mut self, value: &serde_json::Value) {
        write_value(&mut self.stream, value).await;
    }

    /// The next reply; events are never expected here (nothing is subscribed).
    pub async fn reply(&mut self) -> ReplyEnvelope {
        let bytes = guard(read_frame(&mut self.stream))
            .await
            .expect("a reply frame");
        match serde_json::from_slice(&bytes).unwrap() {
            ServerFrame::Reply(reply) => reply,
            ServerFrame::Event(event) => panic!("unexpected event: {event:?}"),
        }
    }
}

pub fn envelope(request_id: &str, body: RequestBody) -> RequestEnvelope {
    RequestEnvelope {
        request_id: RequestId::new(request_id).unwrap(),
        session_id: SessionId::new("contracts-session").unwrap(),
        body,
    }
}

pub fn restart_request(
    epoch: InstanceEpoch,
    request_id: &str,
    operation_id: &str,
) -> RequestEnvelope {
    envelope(
        request_id,
        RequestBody::Command(Command::Lifecycle(LifecycleCommand::Restart(
            RestartCommand::Request(RequestRestart {
                binding: RestartBinding {
                    target_id: id("current"),
                    expected_core_epoch: epoch,
                    target_revision: Revision(2),
                    operation_id: id(operation_id),
                },
                confirmed: true,
            }),
        ))),
    )
}

pub fn cancel_request(
    epoch: InstanceEpoch,
    request_id: &str,
    operation_id: &str,
) -> RequestEnvelope {
    envelope(
        request_id,
        RequestBody::Command(Command::Lifecycle(LifecycleCommand::Restart(
            RestartCommand::Cancel(CancelRestart {
                restart_id: id("restart"),
                expected_core_epoch: epoch,
                expected_revision: Revision(1),
                operation_id: id(operation_id),
            }),
        ))),
    )
}

pub fn query_request(request_id: &str, query: LifecycleQuery) -> RequestEnvelope {
    envelope(request_id, RequestBody::Query(Query::Lifecycle(query)))
}

pub fn status_request(request_id: &str) -> RequestEnvelope {
    query_request(request_id, LifecycleQuery::Restart(RestartQuery::Status))
}

pub fn receipt(reply: &ReplyEnvelope) -> AdminReceipt {
    match &reply.result {
        ReplyResult::Success(ReplyPayload::Command(c)) => match c.result.as_deref() {
            Some(CommandResult::Lifecycle(LifecycleResult::Accepted(receipt))) => receipt.clone(),
            other => panic!("not a lifecycle receipt: {other:?}"),
        },
        other => panic!("not a command success: {other:?}"),
    }
}

pub fn lifecycle_view(reply: &ReplyEnvelope) -> LifecycleReply {
    match &reply.result {
        ReplyResult::Success(ReplyPayload::Query(QueryReply::Lifecycle(view))) => view.clone(),
        other => panic!("not a lifecycle view: {other:?}"),
    }
}

pub fn error_code(reply: &ReplyEnvelope) -> ErrorCode {
    match &reply.result {
        ReplyResult::Error(error) => error.code,
        other => panic!("the request was accepted: {other:?}"),
    }
}

pub fn error_message(reply: &ReplyEnvelope) -> String {
    match &reply.result {
        ReplyResult::Error(error) => error.message.clone(),
        other => panic!("the request was accepted: {other:?}"),
    }
}
