//! A core that speaks the workbench protocol over a real Unix socket and
//! answers the restart administration. Everything it saw is kept, and tests
//! wait on what it saw (never on time).
#![allow(dead_code)]

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Notify, mpsc},
    task::JoinHandle,
};
use workbench_protocol::{
    ErrorCode, RetryAdvice, WorkbenchError,
    commands::{Command, CommandDisposition, CommandReply, CommandResult},
    envelope::{
        Control, ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope,
        ServerFrame,
    },
    events::{EventCursor, EventEnvelope, SubscriptionStarted, UiEvent},
    framing::{FrameDecoder, decode_payload, encode_frame},
    handshake::{
        ActivityCounts, BuildInfo, Capability, ClientHello, CoreState, CoreStatus, HandshakeReply,
        ProtocolVersion, ServerHello, WireLimits,
    },
    ids::{CoreIdentity, RestartId, SnapshotId, SubscriptionId},
    namespaces::lifecycle::{
        AdminId, AdminOperationState, AdminOutcome, AdminReceipt, LifecycleCommand, LifecycleEvent,
        LifecycleQuery, LifecycleReply, LifecycleResult, RestartPhase,
        restart::{
            AdminAuditDiagnostics, RestartCommand, RestartManagementStatus, RestartQuery,
            RestartReply, RestartStatus, RestartTarget, RestartTargets, ShellBlockers,
        },
    },
    queries::{Query, QueryReply},
    wire::{InstanceEpoch, Revision, Sequence},
};

#[derive(Clone, Debug)]
pub struct Seen {
    pub conn: usize,
    pub epoch: u64,
    pub body: RequestBody,
}

#[derive(Clone, Debug)]
pub enum OnRequest {
    /// Accept the restart: a pending receipt.
    Accept,
    /// Read the request and close the connection without a reply: the ACK is lost.
    DropConnection,
    Reject(WorkbenchError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscribeMode {
    /// Replay when the cursor is of this epoch, a snapshot otherwise.
    Honest,
    /// Always answer with a replay, as a broken successor would.
    AlwaysReplay,
}

pub struct State {
    pub phase: RestartPhase,
    pub revision: u64,
    pub restart_id: Option<RestartId>,
    pub audit_warning: bool,
    pub on_request: OnRequest,
    pub subscribe_mode: SubscribeMode,
    pub blockers: Option<ShellBlockers>,
    pub operations: HashMap<String, AdminReceipt>,
    pub seen: Vec<Seen>,
    pub served_status: Vec<RestartPhase>,
    pub subscribes: Vec<(usize, Option<EventCursor>, bool)>,
}

pub struct FakeCore {
    pub epoch: u64,
    path: PathBuf,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

struct Shared {
    epoch: u64,
    capabilities: Vec<Capability>,
    state: Mutex<State>,
    changed: Notify,
    connections: Mutex<Vec<(usize, mpsc::UnboundedSender<Vec<u8>>, JoinHandle<()>)>>,
    subscription_ids: Mutex<HashMap<usize, Vec<SubscriptionId>>>,
    sequence: Mutex<u64>,
}

fn build() -> BuildInfo {
    BuildInfo {
        name: "fake-core".into(),
        version: "0".into(),
        revision: None,
    }
}

pub fn id(text: &str) -> AdminId {
    AdminId::new(text).unwrap()
}

pub fn cursor(epoch: u64, sequence: u64) -> EventCursor {
    EventCursor {
        core_epoch: InstanceEpoch(epoch),
        sequence: Sequence(sequence),
    }
}

impl FakeCore {
    pub fn start(path: &Path, epoch: u64, capabilities: Vec<Capability>) -> Self {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).expect("bind the fake core socket");
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let shared = Arc::new(Shared {
            epoch,
            capabilities,
            state: Mutex::new(State {
                phase: RestartPhase::Running,
                revision: 1,
                restart_id: None,
                audit_warning: false,
                on_request: OnRequest::Accept,
                subscribe_mode: SubscribeMode::Honest,
                blockers: None,
                operations: HashMap::new(),
                seen: Vec::new(),
                served_status: Vec::new(),
                subscribes: Vec::new(),
            }),
            changed: Notify::new(),
            connections: Mutex::new(Vec::new()),
            subscription_ids: Mutex::new(HashMap::new()),
            sequence: Mutex::new(1),
        });
        let accept = tokio::spawn({
            let shared = shared.clone();
            async move {
                let mut next = 0usize;
                while let Ok((stream, _)) = listener.accept().await {
                    next += 1;
                    let (tx, rx) = mpsc::unbounded_channel();
                    let task = tokio::spawn(serve(stream, shared.clone(), next, rx));
                    shared.connections.lock().unwrap().push((next, tx, task));
                }
            }
        });
        Self {
            epoch,
            path: path.to_owned(),
            shared,
            accept,
        }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until `ready` holds. Re-checked after every request the core handles.
    pub async fn wait_for(&self, ready: impl Fn(&State) -> bool) {
        loop {
            let notified = self.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if ready(&self.state()) {
                return;
            }
            notified.await;
        }
    }

    pub fn commands(&self) -> Vec<Command> {
        self.state()
            .seen
            .iter()
            .filter_map(|s| match &s.body {
                RequestBody::Command(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn restart_commands(&self) -> Vec<RestartCommand> {
        self.commands()
            .into_iter()
            .filter_map(|c| match c {
                Command::Lifecycle(LifecycleCommand::Restart(r)) => Some(r),
                _ => None,
            })
            .collect()
    }

    /// Change the phase and tell every subscriber the management state changed.
    pub fn set_phase(&self, phase: RestartPhase) {
        {
            let mut state = self.state();
            state.phase = phase;
            state.revision += 1;
        }
        self.push(UiEvent::Lifecycle(LifecycleEvent::AdminChanged {
            revision: Revision(self.state().revision),
            restarting: phase != RestartPhase::Running,
        }));
    }

    pub fn complete_operation(&self, op: &str, outcome: AdminOutcome) {
        let mut state = self.state();
        let audit_warning = state.audit_warning;
        state.operations.insert(
            op.into(),
            AdminReceipt {
                operation_id: id(op),
                state: AdminOperationState::Completed,
                outcome,
                audit_warning,
            },
        );
    }

    /// Push an event to every live subscription.
    pub fn push(&self, event: UiEvent) {
        let subs = self.shared.subscription_ids.lock().unwrap().clone();
        let connections = self.shared.connections.lock().unwrap();
        for (conn, tx, _) in connections.iter() {
            for subscription_id in subs.get(conn).into_iter().flatten() {
                let sequence = {
                    let mut s = self.shared.sequence.lock().unwrap();
                    *s += 1;
                    *s
                };
                let frame = ServerFrame::Event(Box::new(EventEnvelope {
                    subscription_id: subscription_id.clone(),
                    cursor: cursor(self.epoch, sequence),
                    caused_by_request_id: None,
                    event: event.clone(),
                }));
                let _ = tx.send(encode_frame(&frame, u32::MAX).unwrap());
            }
        }
    }

    /// The core goes away: every connection ends and the socket disappears.
    pub fn stop_by_ref(&self) {
        self.accept.abort();
        for (_, _, task) in self.shared.connections.lock().unwrap().drain(..) {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }

    pub fn stop(self) {
        self.accept.abort();
        for (_, _, task) in self.shared.connections.lock().unwrap().drain(..) {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn hello(shared: &Shared) -> ServerHello {
    ServerHello {
        protocol_version: ProtocolVersion::CURRENT,
        capabilities: shared.capabilities.clone(),
        server_build: build(),
        core: CoreIdentity::new("fake-core").unwrap(),
        instance_epoch: InstanceEpoch(shared.epoch),
        limits: WireLimits::default(),
        status: CoreStatus {
            state: CoreState::Ready,
            retention_reasons: vec![],
            running: ActivityCounts::default(),
        },
    }
}

fn management_status(shared: &Shared, state: &State) -> RestartManagementStatus {
    RestartManagementStatus {
        audit: AdminAuditDiagnostics::default(),
        core_epoch: InstanceEpoch(shared.epoch),
        status: RestartStatus {
            restart_id: state.restart_id.clone(),
            phase: state.phase,
            revision: state.revision,
            blockers: vec![],
            blocker_total: 0,
            blocker_omitted: 0,
            active_producers: 0,
        },
    }
}

fn lifecycle_reply(reply: LifecycleReply) -> ReplyResult {
    ReplyResult::Success(ReplyPayload::Query(QueryReply::Lifecycle(reply)))
}

enum Reaction {
    Reply(ReplyResult),
    Close,
}

fn react(shared: &Shared, conn: usize, request: &RequestEnvelope) -> Reaction {
    let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
    state.seen.push(Seen {
        conn,
        epoch: shared.epoch,
        body: request.body.clone(),
    });
    let reaction = match &request.body {
        RequestBody::Control(Control::Subscribe(subscribe)) => {
            let replay = match state.subscribe_mode {
                SubscribeMode::AlwaysReplay => true,
                SubscribeMode::Honest => subscribe
                    .after
                    .is_some_and(|c| c.core_epoch.get() == shared.epoch),
            };
            state.subscribes.push((conn, subscribe.after, !replay));
            let mut ids = shared.subscription_ids.lock().unwrap();
            let sid =
                SubscriptionId::new(format!("s{conn}-{}", ids.entry(conn).or_default().len()))
                    .unwrap();
            ids.entry(conn).or_default().push(sid.clone());
            Reaction::Reply(ReplyResult::Success(ReplyPayload::SubscriptionStarted(
                SubscriptionStarted {
                    subscription_id: sid,
                    snapshot_id: (!replay).then(|| SnapshotId::new("snap").unwrap()),
                    high_water: cursor(shared.epoch, 1),
                    initial_page: None,
                },
            )))
        }
        RequestBody::Control(Control::Unsubscribe { subscription_id }) => {
            Reaction::Reply(ReplyResult::Success(ReplyPayload::Unsubscribed {
                subscription_id: subscription_id.clone(),
            }))
        }
        RequestBody::Query(Query::Lifecycle(LifecycleQuery::Restart(RestartQuery::Targets))) => {
            Reaction::Reply(lifecycle_reply(LifecycleReply::Restart(
                RestartReply::Targets(RestartTargets {
                    core_epoch: InstanceEpoch(shared.epoch),
                    revision: Revision(4),
                    targets: vec![RestartTarget {
                        id: id("current"),
                        revision: Revision(11),
                    }],
                    truncated: false,
                }),
            )))
        }
        RequestBody::Query(Query::Lifecycle(LifecycleQuery::Restart(RestartQuery::Status))) => {
            let phase = state.phase;
            state.served_status.push(phase);
            Reaction::Reply(lifecycle_reply(LifecycleReply::Restart(
                RestartReply::Status(management_status(shared, &state)),
            )))
        }
        RequestBody::Query(Query::Lifecycle(LifecycleQuery::Restart(
            RestartQuery::ShellBlockers,
        ))) => Reaction::Reply(lifecycle_reply(LifecycleReply::Restart(
            RestartReply::ShellBlockers(state.blockers.clone().unwrap_or(ShellBlockers {
                blockers: vec![],
                total: Revision(0),
                omitted: Revision(0),
            })),
        ))),
        RequestBody::Query(Query::Lifecycle(LifecycleQuery::OperationStatus { operation_id })) => {
            match state.operations.get(operation_id.as_str()) {
                Some(receipt) => Reaction::Reply(lifecycle_reply(LifecycleReply::OperationStatus(
                    receipt.clone(),
                ))),
                None => Reaction::Reply(ReplyResult::Error(WorkbenchError::new(
                    ErrorCode::NotFound,
                    "unknown operation",
                    RetryAdvice::Never,
                ))),
            }
        }
        RequestBody::Command(Command::Lifecycle(command)) => {
            let operation_id = command.operation_id().clone();
            match state.on_request.clone() {
                OnRequest::DropConnection => Reaction::Close,
                OnRequest::Reject(error) => Reaction::Reply(ReplyResult::Error(error)),
                OnRequest::Accept => {
                    if let LifecycleCommand::Restart(RestartCommand::Request(_)) = command {
                        state.restart_id = Some(RestartId::new("restart-1").unwrap());
                    }
                    let receipt = AdminReceipt {
                        operation_id: operation_id.clone(),
                        state: AdminOperationState::Pending,
                        outcome: AdminOutcome::Pending,
                        audit_warning: state.audit_warning,
                    };
                    state
                        .operations
                        .insert(operation_id.as_str().into(), receipt.clone());
                    Reaction::Reply(ReplyResult::Success(ReplyPayload::Command(CommandReply {
                        disposition: CommandDisposition::Accepted,
                        submission_id: None,
                        run_id: None,
                        thread_revision: None,
                        result: Some(Box::new(CommandResult::Lifecycle(
                            LifecycleResult::Accepted(receipt),
                        ))),
                    })))
                }
            }
        }
        RequestBody::Control(Control::Ping { nonce }) => {
            Reaction::Reply(ReplyResult::Success(ReplyPayload::Pong { nonce: *nonce }))
        }
        _ => Reaction::Reply(ReplyResult::Error(WorkbenchError::new(
            ErrorCode::UnsupportedCapability,
            "not served by the fake core",
            RetryAdvice::Never,
        ))),
    };
    drop(state);
    shared.changed.notify_waiters();
    reaction
}

async fn serve(
    stream: UnixStream,
    shared: Arc<Shared>,
    conn: usize,
    mut outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let (mut read, mut write) = stream.into_split();
    let mut decoder = FrameDecoder::new(u32::MAX);
    let mut buffer = [0u8; 4096];
    let mut accepted = false;
    loop {
        tokio::select! {
            frame = outgoing.recv() => {
                let Some(frame) = frame else { return };
                if write.write_all(&frame).await.is_err() { return }
            }
            read = read.read(&mut buffer) => {
                let Ok(n) = read else { return };
                if n == 0 { return }
                decoder.push(&buffer[..n]);
                while let Ok(Some(payload)) = decoder.next_payload() {
                    if !accepted {
                        let _: ClientHello = decode_payload(&payload).expect("a hello first");
                        let reply = encode_frame(&HandshakeReply::Accepted(hello(&shared)), u32::MAX).unwrap();
                        if write.write_all(&reply).await.is_err() { return }
                        accepted = true;
                        continue;
                    }
                    let request: RequestEnvelope = decode_payload(&payload).expect("a request");
                    match react(&shared, conn, &request) {
                        Reaction::Close => return,
                        Reaction::Reply(result) => {
                            let frame = ServerFrame::Reply(ReplyEnvelope { request_id: request.request_id, result });
                            if write.write_all(&encode_frame(&frame, u32::MAX).unwrap()).await.is_err() { return }
                        }
                    }
                }
            }
        }
    }
}
