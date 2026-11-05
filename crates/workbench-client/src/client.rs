//! The connection: handshake, a single writer, a reader that routes replies
//! and events, and the rules for what a lost connection means for each request.

use crate::error::{ClientError, TransportError};
use crate::subscription::{
    Queued, Subscription, SubscriptionEnd, SubscriptionEntry, SubscriptionGuard, SubscriptionParts,
};
use crate::transport::{FrameSink, FrameSource, Transport, framing_error};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use workbench_protocol::commands::{Command, CommandReply};
use workbench_protocol::envelope::{
    Control, ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope, ServerFrame,
};
use workbench_protocol::events::{CursorDecision, CursorTracker, EventEnvelope, Subscribe};
use workbench_protocol::framing::{decode_payload, encode_frame};
use workbench_protocol::handshake::{
    BuildInfo, Capability, ClientHello, HandshakeReply, ProtocolVersion, ServerHello,
};
use workbench_protocol::ids::{ClientId, CoreIdentity, RequestId, SessionId, SubscriptionId};
use workbench_protocol::limits::DEFAULT_FRAME_BYTES;
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::validation::ValidationError;
use workbench_protocol::wire::Nonce;

/// Longest client id: request ids are `<client id>.<number>` and ids are capped at 128 bytes.
const MAX_CLIENT_ID_BYTES: usize = 96;

/// How to introduce this client to a core.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub client_instance_id: ClientId,
    pub client_build: BuildInfo,
    pub supported_versions: Vec<ProtocolVersion>,
    pub required_capabilities: Vec<Capability>,
    pub optional_capabilities: Vec<Capability>,
    /// Refuse any core but this one.
    pub desired_core: Option<CoreIdentity>,
    /// Numbers request ids. Reuse one config (or clone it) across reconnects so
    /// request ids never repeat for the same client instance.
    pub request_counter: Arc<AtomicU64>,
}

impl ClientConfig {
    /// Requires commands and queries; asks for everything else when offered.
    pub fn new(client_instance_id: ClientId, client_build: BuildInfo) -> Self {
        Self {
            client_instance_id,
            client_build,
            supported_versions: vec![ProtocolVersion::CURRENT],
            required_capabilities: vec![Capability::Commands, Capability::Queries],
            optional_capabilities: vec![
                Capability::SnapshotEvents,
                Capability::InputReceipts,
                Capability::Approvals,
                Capability::Questions,
                Capability::CommittedBodies,
            ],
            desired_core: None,
            request_counter: Arc::new(AtomicU64::new(1)),
        }
    }
}

/// Whether the connection is usable. There is no automatic reconnect or
/// restart in this layer; a closed client stays closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Closed(TransportError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingKind {
    Command,
    Query,
    Subscribe,
    Control,
    /// A ping; the reply must echo this nonce.
    Ping(Nonce),
    /// Nobody waits for the reply (best-effort unsubscribe).
    Discard,
}

pub(crate) enum Completed {
    Payload(ReplyPayload),
    Subscription(SubscriptionParts),
}

type Waiter = oneshot::Sender<Result<Completed, ClientError>>;

struct Pending {
    kind: PendingKind,
    /// The writer has begun handing this request's frame to the transport, so
    /// the core may have received it.
    started: bool,
    waiter: Option<Waiter>,
}

struct Outgoing {
    request_id: RequestId,
    frame: Vec<u8>,
}

#[derive(Default)]
struct State {
    pending: HashMap<RequestId, Pending>,
    subscriptions: HashMap<SubscriptionId, SubscriptionEntry>,
    closed: Option<TransportError>,
    /// Numbers each registration so a stale owner of a reused id can be told apart.
    next_generation: u64,
}

pub(crate) struct Shared {
    hello: ServerHello,
    client_id: ClientId,
    session_id: SessionId,
    counter: Arc<AtomicU64>,
    state: Mutex<State>,
    writer: mpsc::Sender<Outgoing>,
    connection: watch::Sender<ConnectionState>,
}

pub(crate) struct Inner {
    pub(crate) shared: Arc<Shared>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

fn violation(message: impl Into<String>) -> TransportError {
    TransportError::ProtocolViolation(message.into())
}

impl Shared {
    fn new(
        hello: ServerHello,
        client_id: ClientId,
        session_id: SessionId,
        counter: Arc<AtomicU64>,
        writer: mpsc::Sender<Outgoing>,
    ) -> Arc<Self> {
        let (connection, _) = watch::channel(ConnectionState::Connected);
        Arc::new(Self {
            hello,
            client_id,
            session_id,
            counter,
            state: Mutex::new(State::default()),
            writer,
            connection,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue one request. Runs entirely before any await, so once the caller
    /// has polled its future the request is committed to being sent even if
    /// the future is dropped afterwards.
    fn submit(
        &self,
        body: RequestBody,
        kind: PendingKind,
    ) -> Result<Option<oneshot::Receiver<Result<Completed, ClientError>>>, ClientError> {
        if let Some(capability) = body.required_capability()
            && !self.hello.has(capability)
        {
            return Err(ClientError::UnsupportedCapability(capability));
        }
        let limits = &self.hello.limits;
        body.validate(limits)?;

        let mut state = self.lock();
        if let Some(reason) = &state.closed {
            return Err(reason_for_new_request(reason).into());
        }
        if state.pending.len() >= limits.max_in_flight_requests as usize {
            return Err(TransportError::QueueFull.into());
        }
        let number = self.counter.fetch_add(1, Ordering::Relaxed);
        let request_id = RequestId::new(format!("{}.{number}", self.client_id))?;
        let envelope = RequestEnvelope {
            request_id: request_id.clone(),
            session_id: self.session_id.clone(),
            body,
        };
        let frame = encode_frame(&envelope, limits.max_frame_bytes).map_err(framing_error)?;
        let (waiter, receiver) = if kind == PendingKind::Discard {
            (None, None)
        } else {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        };
        state.pending.insert(
            request_id.clone(),
            Pending {
                kind,
                started: false,
                waiter,
            },
        );
        if self
            .writer
            .try_send(Outgoing {
                request_id: request_id.clone(),
                frame,
            })
            .is_err()
        {
            state.pending.remove(&request_id);
            return Err(TransportError::QueueFull.into());
        }
        Ok(receiver)
    }

    fn mark_started(&self, request_id: &RequestId) -> bool {
        match self.lock().pending.get_mut(request_id) {
            Some(pending) => {
                pending.started = true;
                true
            }
            None => false,
        }
    }

    /// Close the connection and settle every request.
    ///
    /// A command the writer had started on may have reached the core, so it
    /// becomes `OutcomeUnknown`; it is never resent. Everything else
    /// definitely did not run, or can be repeated safely.
    pub(crate) fn close(&self, reason: TransportError) {
        let (pending, subscriptions) = {
            let mut state = self.lock();
            if state.closed.is_some() {
                return;
            }
            state.closed = Some(reason.clone());
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.subscriptions),
            )
        };
        for (request_id, entry) in pending {
            let Some(waiter) = entry.waiter else { continue };
            let error = if entry.kind == PendingKind::Command && entry.started {
                TransportError::OutcomeUnknown { request_id }
            } else {
                reason.clone()
            };
            let _ = waiter.send(Err(error.into()));
        }
        for entry in subscriptions.into_values() {
            entry.finish(SubscriptionEnd::Disconnected);
        }
        self.connection
            .send_replace(ConnectionState::Closed(reason));
    }

    /// Unregister generation `generation` of `id`. Returns false, touching
    /// nothing, when that registration is already gone, including when the id
    /// has since been registered again by a newer subscription.
    pub(crate) fn drop_subscription(&self, id: &SubscriptionId, generation: u64) -> bool {
        let mut state = self.lock();
        if state.subscriptions.get(id).map(|entry| entry.generation) != Some(generation) {
            return false;
        }
        let entry = state.subscriptions.remove(id).expect("present");
        drop(state);
        entry.finish(SubscriptionEnd::Unsubscribed);
        true
    }

    /// Stop a subscription locally and tell the core, without waiting.
    pub(crate) fn release_subscription(&self, id: &SubscriptionId, generation: u64) {
        if self.drop_subscription(id, generation) {
            self.unsubscribe_in_background(id);
        }
    }

    fn unsubscribe_in_background(&self, id: &SubscriptionId) {
        let body = RequestBody::Control(Control::Unsubscribe {
            subscription_id: id.clone(),
        });
        // Best effort: if the connection is gone or full there is nothing to tell.
        let _ = self.submit(body, PendingKind::Discard);
    }

    fn handle_frame(self: &Arc<Self>, payload: &[u8]) -> Result<(), TransportError> {
        let frame: ServerFrame = decode_payload(payload)
            .map_err(|error| violation(format!("undecodable frame: {error}")))?;
        // Deserializing only checks shape. Nothing the core sent reaches a
        // caller before its sizes and relationships are checked too.
        frame
            .validate(&self.hello.limits)
            .map_err(|error| violation(format!("frame breaks protocol limits: {error}")))?;
        match frame {
            ServerFrame::Reply(reply) => self.handle_reply(reply),
            ServerFrame::Event(event) => {
                self.handle_event(*event, payload.len());
                Ok(())
            }
        }
    }

    fn handle_reply(self: &Arc<Self>, reply: ReplyEnvelope) -> Result<(), TransportError> {
        let Some(pending) = self.lock().pending.remove(&reply.request_id) else {
            return Err(violation(format!(
                "reply for unknown or already answered request {}",
                reply.request_id
            )));
        };
        let outcome = match reply.result {
            ReplyResult::Error(error) => Ok(Err(ClientError::Server(error))),
            ReplyResult::Success(payload) => self.complete(pending.kind, payload),
        };
        match outcome {
            Ok(result) => {
                if let Some(waiter) = pending.waiter {
                    let _ = waiter.send(result);
                }
                Ok(())
            }
            Err(error) => {
                if let Some(waiter) = pending.waiter {
                    let _ = waiter.send(Err(error.clone().into()));
                }
                Err(error)
            }
        }
    }

    /// Check that the reply type fits the request type and build the result.
    fn complete(
        self: &Arc<Self>,
        kind: PendingKind,
        payload: ReplyPayload,
    ) -> Result<Result<Completed, ClientError>, TransportError> {
        let fits = match (kind, &payload) {
            (PendingKind::Command, ReplyPayload::Command(_))
            | (PendingKind::Query, ReplyPayload::Query(_))
            | (PendingKind::Subscribe, ReplyPayload::SubscriptionStarted(_))
            | (
                PendingKind::Control | PendingKind::Discard,
                ReplyPayload::Unsubscribed { .. } | ReplyPayload::Ack,
            ) => true,
            (PendingKind::Ping(sent), ReplyPayload::Pong { nonce }) => *nonce == sent,
            _ => false,
        };
        if !fits {
            return Err(violation(
                "reply does not match the request it answers (type or ping nonce)",
            ));
        }
        let ReplyPayload::SubscriptionStarted(started) = payload else {
            return Ok(Ok(Completed::Payload(payload)));
        };

        let limits = &self.hello.limits;
        let (tx, rx) = mpsc::channel(limits.subscription_queue_events as usize);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let end = Arc::new(Mutex::new(None));
        let generation;
        {
            let mut state = self.lock();
            // Checked under the same lock that registers the entry: `close`
            // takes this lock to settle subscriptions, so an entry can never
            // be added after it and left waiting on a dead connection.
            if let Some(reason) = &state.closed {
                return Ok(Err(reason_for_new_request(reason).into()));
            }
            if state.subscriptions.contains_key(&started.subscription_id) {
                return Err(violation("subscription id is already in use"));
            }
            // Registered before the reader looks at the next frame, so no
            // event for this subscription can slip past it.
            state.next_generation += 1;
            generation = state.next_generation;
            state.subscriptions.insert(
                started.subscription_id.clone(),
                SubscriptionEntry {
                    generation,
                    tx,
                    tracker: CursorTracker::new(started.high_water),
                    queued_bytes: queued_bytes.clone(),
                    end: end.clone(),
                },
            );
        }
        Ok(Ok(Completed::Subscription(SubscriptionParts {
            guard: SubscriptionGuard::new(
                self.clone(),
                started.subscription_id.clone(),
                generation,
            ),
            started,
            rx,
            queued_bytes,
            end,
        })))
    }

    fn handle_event(&self, event: EventEnvelope, frame_bytes: usize) {
        let id = event.subscription_id.clone();
        let mut state = self.lock();
        let Some(entry) = state.subscriptions.get_mut(&id) else {
            // Raced with an unsubscribe or a subscription that already ended.
            return;
        };
        match entry.tracker.observe(event.cursor) {
            CursorDecision::Duplicate => return,
            CursorDecision::EpochMismatch => {
                let entry = state.subscriptions.remove(&id).expect("present");
                drop(state);
                entry.finish(SubscriptionEnd::EpochChanged);
                self.unsubscribe_in_background(&id);
                return;
            }
            CursorDecision::Deliver => {}
        }
        let ends = event.event.ends_subscription();
        let limit = self.hello.limits.subscription_queue_bytes as usize;
        let before = entry.queued_bytes.fetch_add(frame_bytes, Ordering::Relaxed);
        let overflow = before + frame_bytes > limit
            || entry
                .tx
                .try_send(Queued {
                    envelope: event,
                    bytes: frame_bytes,
                })
                .is_err();
        if overflow {
            let entry = state.subscriptions.remove(&id).expect("present");
            drop(state);
            entry.finish(SubscriptionEnd::SlowSubscriber);
            self.unsubscribe_in_background(&id);
        } else if ends {
            let entry = state.subscriptions.remove(&id).expect("present");
            drop(state);
            entry.finish(SubscriptionEnd::EndedByCore);
        }
    }
}

fn reason_for_new_request(reason: &TransportError) -> TransportError {
    match reason {
        TransportError::OutcomeUnknown { .. } => TransportError::Disconnected,
        other => other.clone(),
    }
}

async fn write_loop<S: FrameSink>(
    mut sink: S,
    mut requests: mpsc::Receiver<Outgoing>,
    shared: Arc<Shared>,
) {
    while let Some(outgoing) = requests.recv().await {
        if !shared.mark_started(&outgoing.request_id) {
            // The connection closed and settled this request already.
            continue;
        }
        if let Err(error) = sink.send_frame(&outgoing.frame).await {
            shared.close(error);
            return;
        }
    }
}

async fn read_loop<S: FrameSource>(mut source: S, shared: Arc<Shared>) {
    loop {
        match source.recv_frame().await {
            Ok(Some(payload)) => {
                if let Err(error) = shared.handle_frame(&payload) {
                    shared.close(error);
                    return;
                }
            }
            Ok(None) => {
                shared.close(TransportError::Disconnected);
                return;
            }
            Err(error) => {
                shared.close(error);
                return;
            }
        }
    }
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A connection to a core. Cheap to clone; all clones share the connection.
#[derive(Clone)]
pub struct WorkbenchClient {
    inner: Arc<Inner>,
}

impl WorkbenchClient {
    /// Run the handshake over `transport` and start the connection tasks.
    ///
    /// The first frame sent is the [`ClientHello`]; nothing else is sent until
    /// the core has accepted it. Requires a Tokio runtime.
    pub async fn connect<T: Transport>(
        transport: T,
        config: ClientConfig,
    ) -> Result<Self, ClientError> {
        if config.client_instance_id.as_str().len() > MAX_CLIENT_ID_BYTES {
            return Err(ValidationError::TooLong {
                field: "client_instance_id",
                max: MAX_CLIENT_ID_BYTES,
                actual: config.client_instance_id.as_str().len(),
            }
            .into());
        }
        let hello = ClientHello {
            supported_versions: config.supported_versions.clone(),
            required_capabilities: config
                .required_capabilities
                .iter()
                .map(|c| (*c).into())
                .collect(),
            optional_capabilities: config
                .optional_capabilities
                .iter()
                .map(|c| (*c).into())
                .collect(),
            client_build: config.client_build.clone(),
            client_instance_id: config.client_instance_id.clone(),
            desired_core: config.desired_core.clone(),
        };
        hello.validate()?;

        let (mut sink, mut source) = transport.split();
        source.set_max_frame_bytes(DEFAULT_FRAME_BYTES);
        let frame = encode_frame(&hello, DEFAULT_FRAME_BYTES).map_err(framing_error)?;
        sink.send_frame(&frame).await?;
        let payload = source
            .recv_frame()
            .await?
            .ok_or(TransportError::Disconnected)?;
        let reply: HandshakeReply = decode_payload(&payload)
            .map_err(|error| violation(format!("undecodable handshake reply: {error}")))?;
        let server = match reply {
            HandshakeReply::Rejected(rejected) => {
                rejected
                    .validate()
                    .map_err(|error| violation(format!("unusable handshake rejection: {error}")))?;
                return Err(ClientError::HandshakeRejected(rejected));
            }
            HandshakeReply::Accepted(server) => server,
        };
        check_accepted(&config, &server)?;

        source.set_max_frame_bytes(server.limits.max_frame_bytes);
        let session_id = SessionId::new(format!(
            "{}.s{}",
            config.client_instance_id,
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))?;
        // Room for every allowed in-flight request plus background unsubscribes.
        let (writer_tx, writer_rx) =
            mpsc::channel(server.limits.max_in_flight_requests as usize + 8);
        let shared = Shared::new(
            server,
            config.client_instance_id,
            session_id,
            config.request_counter,
            writer_tx,
        );
        let writer = tokio::spawn(write_loop(sink, writer_rx, shared.clone()));
        let reader = tokio::spawn(read_loop(source, shared.clone()));
        Ok(Self {
            inner: Arc::new(Inner {
                shared,
                reader,
                writer,
            }),
        })
    }

    /// What the core said about itself when it accepted this connection.
    pub fn server_hello(&self) -> &ServerHello {
        &self.inner.shared.hello
    }

    pub fn session_id(&self) -> &SessionId {
        &self.inner.shared.session_id
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.inner.shared.connection.borrow().clone()
    }

    /// Follow connection state changes.
    pub fn watch_connection(&self) -> watch::Receiver<ConnectionState> {
        self.inner.shared.connection.subscribe()
    }

    /// Wait until the connection is closed and say why.
    pub async fn closed(&self) -> TransportError {
        let mut state = self.watch_connection();
        match state
            .wait_for(|s| matches!(s, ConnectionState::Closed(_)))
            .await
        {
            Ok(guard) => match &*guard {
                ConnectionState::Closed(reason) => reason.clone(),
                ConnectionState::Connected => unreachable!("waited for Closed"),
            },
            Err(_) => TransportError::Disconnected,
        }
    }

    /// Send a command and wait for the core's answer.
    ///
    /// Once this future has been polled the request is queued and will be sent
    /// even if the future is dropped: dropping does not cancel a command the
    /// core may already have received. If the connection is lost while the
    /// reply is outstanding the result is [`TransportError::OutcomeUnknown`]
    /// and the command is not resent.
    pub async fn command(&self, command: Command) -> Result<CommandReply, ClientError> {
        match self
            .inner
            .call(RequestBody::Command(command), PendingKind::Command)
            .await?
        {
            Completed::Payload(ReplyPayload::Command(reply)) => Ok(reply),
            _ => Err(violation("unexpected reply to a command").into()),
        }
    }

    /// Send a read-only query and wait for the answer.
    pub async fn query(&self, query: Query) -> Result<QueryReply, ClientError> {
        match self
            .inner
            .call(RequestBody::Query(query), PendingKind::Query)
            .await?
        {
            Completed::Payload(ReplyPayload::Query(reply)) => Ok(reply),
            _ => Err(violation("unexpected reply to a query").into()),
        }
    }

    /// Start an event subscription.
    pub async fn subscribe(&self, subscribe: Subscribe) -> Result<Subscription, ClientError> {
        let body = RequestBody::Control(Control::Subscribe(subscribe));
        match self.inner.call(body, PendingKind::Subscribe).await? {
            Completed::Subscription(parts) => Ok(Subscription::new(self.inner.clone(), parts)),
            Completed::Payload(_) => Err(violation("unexpected reply to a subscribe").into()),
        }
    }

    /// Round-trip a ping, to check that the core is still answering.
    pub async fn ping(&self, nonce: Nonce) -> Result<(), ClientError> {
        let body = RequestBody::Control(Control::Ping { nonce });
        match self.inner.call(body, PendingKind::Ping(nonce)).await? {
            Completed::Payload(ReplyPayload::Pong { .. }) => Ok(()),
            _ => Err(violation("unexpected reply to a ping").into()),
        }
    }
}

impl Inner {
    /// Queue a request and wait for its reply. Nothing awaits before the
    /// request is queued, which is what makes dropping the future harmless.
    pub(crate) async fn call(
        &self,
        body: RequestBody,
        kind: PendingKind,
    ) -> Result<Completed, ClientError> {
        let receiver = self
            .shared
            .submit(body, kind)?
            .expect("only discarded requests have no waiter");
        receiver
            .await
            .unwrap_or_else(|_| Err(TransportError::Disconnected.into()))
    }
}

fn check_accepted(config: &ClientConfig, server: &ServerHello) -> Result<(), TransportError> {
    if !config.supported_versions.contains(&server.protocol_version) {
        return Err(violation(
            "core chose a protocol version the client did not offer",
        ));
    }
    if let Some(missing) = config
        .required_capabilities
        .iter()
        .find(|c| !server.has(**c))
    {
        return Err(violation(format!(
            "core accepted without required capability {}",
            missing.name()
        )));
    }
    if let Some(desired) = &config.desired_core
        && *desired != server.core
    {
        return Err(violation("core identity is not the one requested"));
    }
    // Build identity, status and limits: nothing is built on a hello that breaks them.
    server
        .validate()
        .map_err(|error| violation(format!("core sent an unusable hello: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::events::{EventCursor, SubscriptionStarted};
    use workbench_protocol::handshake::{ActivityCounts, CoreState, CoreStatus, WireLimits};
    use workbench_protocol::wire::{InstanceEpoch, Sequence};

    fn shared() -> (Arc<Shared>, mpsc::Receiver<Outgoing>) {
        let build = BuildInfo {
            name: "test".into(),
            version: "0".into(),
            revision: None,
        };
        let hello = ServerHello {
            protocol_version: ProtocolVersion::CURRENT,
            capabilities: Capability::ALL.to_vec(),
            server_build: build,
            core: CoreIdentity::new("core").unwrap(),
            instance_epoch: InstanceEpoch(1),
            limits: WireLimits::default(),
            status: CoreStatus {
                state: CoreState::Ready,
                retention_reasons: vec![],
                running: ActivityCounts::default(),
            },
        };
        let (tx, rx) = mpsc::channel(8);
        let shared = Shared::new(
            hello,
            ClientId::new("c").unwrap(),
            SessionId::new("s").unwrap(),
            Arc::new(AtomicU64::new(1)),
            tx,
        );
        (shared, rx)
    }

    fn started(id: &str) -> ReplyPayload {
        ReplyPayload::SubscriptionStarted(SubscriptionStarted {
            subscription_id: SubscriptionId::new(id).unwrap(),
            snapshot_id: None,
            high_water: EventCursor {
                core_epoch: InstanceEpoch(1),
                sequence: Sequence(0),
            },
            initial_page: None,
        })
    }

    /// The reader can be in the middle of a subscribe reply when another task
    /// closes the connection. Whichever side takes the state lock first, no
    /// registration may outlive the close: both orders are replayed here
    /// deterministically.
    #[tokio::test]
    async fn a_subscription_cannot_be_registered_after_the_connection_closed() {
        let (shared, _outgoing) = shared();
        shared.close(TransportError::Disconnected);
        let result = shared.complete(PendingKind::Subscribe, started("late"));
        assert!(
            matches!(
                result,
                Ok(Err(ClientError::Transport(TransportError::Disconnected)))
            ),
            "{:?}",
            result.map(|r| r.map(|_| ()))
        );
        assert!(shared.lock().subscriptions.is_empty());
    }

    #[tokio::test]
    async fn a_subscription_registered_before_the_close_is_settled_by_it() {
        let (shared, _outgoing) = shared();
        let Ok(Ok(Completed::Subscription(parts))) =
            shared.complete(PendingKind::Subscribe, started("early"))
        else {
            panic!("registration should succeed on a live connection");
        };
        let end = parts.end.clone();
        shared.close(TransportError::Disconnected);
        assert!(shared.lock().subscriptions.is_empty());
        assert_eq!(*end.lock().unwrap(), Some(SubscriptionEnd::Disconnected));
        // Releasing after the close finds nothing to release and sends nothing.
        drop(parts);
    }
}
