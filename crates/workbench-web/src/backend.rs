//! What the web adapter needs from the core.
//!
//! The adapter never depends on the core. The core binary implements
//! [`WorkbenchBackend`] on top of `workbench-host` and hands it to
//! [`WebServer::start`](crate::WebServer::start). The trait only uses
//! `workbench-protocol` types, so a fake backend is enough to test the adapter.
//!
//! # Secret masking
//!
//! Secret masking (`secret-guard`, ADR 0045 §5) belongs to the backend. Every
//! reply and event a backend returns must already be masked: this crate does
//! not depend on `secret-guard`, relays protocol values unchanged and never
//! inspects their text.

use std::future::Future;
use tokio::sync::mpsc;
use workbench_protocol::envelope::{Provenance, ReplyEnvelope, RequestEnvelope};
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::events::{EventEnvelope, Subscribe, SubscriptionStarted};
use workbench_protocol::handshake::ServerHello;
use workbench_protocol::ids::{ClientId, SessionId};

/// The web client a request came from, as the adapter's ingress resolved it.
///
/// There is no authentication (ADR 0045 §5), so these ids only group requests
/// of one browser tab; they carry no privilege. They are derived from the
/// `session_id` of the request envelope and are not an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSession {
    pub session_id: SessionId,
    /// The id recorded in the [`Provenance`] the adapter attaches.
    pub client_id: ClientId,
}

/// A started subscription: the answer plus the events that follow it.
#[derive(Debug)]
pub struct BackendSubscription {
    /// Snapshot or replay answer. Events at or before `started.high_water` are
    /// never delivered to the browser, even if the backend sends them.
    pub started: SubscriptionStarted,
    /// Events after `started.high_water`, in cursor order.
    ///
    /// The backend must never wait for this channel: when it is full the
    /// backend sends [`UiEvent::ResyncRequired`](workbench_protocol::events::UiEvent)
    /// (reason `SlowSubscriber`) with `try_send` semantics and ends the stream.
    /// An event that ends the subscription ([`UiEvent::ends_subscription`])
    /// must be the last one sent.
    ///
    /// The adapter drops the receiver when the browser disconnects, the server
    /// shuts down or the subscription ends; the backend treats a closed
    /// channel as "unsubscribed". Closing the channel without a terminal event
    /// makes the adapter end the SSE stream, and the browser reconnects with
    /// `Last-Event-ID`.
    ///
    /// [`UiEvent::ends_subscription`]: workbench_protocol::events::UiEvent::ends_subscription
    pub events: mpsc::Receiver<EventEnvelope>,
}

/// The core as seen by the web adapter.
///
/// Futures must be `Send` because the adapter runs on a multi-threaded runtime.
pub trait WorkbenchBackend: Send + Sync + 'static {
    /// Execute one command or query.
    ///
    /// `provenance` is always human on the web surface; it comes from the
    /// adapter's ingress and never from the request body. The reply's
    /// `request_id` must equal the request's; the adapter replaces a mismatch
    /// by an internal error.
    fn handle_request(
        &self,
        session: &WebSession,
        request: RequestEnvelope,
        provenance: Provenance,
    ) -> impl Future<Output = ReplyEnvelope> + Send;

    /// Start a subscription for one SSE connection.
    ///
    /// `client` is unique per SSE connection. The cursor in `subscribe.after`
    /// has already been parsed from `Last-Event-ID` or the `after` parameter.
    fn subscribe(
        &self,
        client: ClientId,
        subscribe: Subscribe,
    ) -> impl Future<Output = Result<BackendSubscription, WorkbenchError>> + Send;

    /// What the core is and how it is doing, for `GET /api/v1/health`: server
    /// build, protocol version, capabilities, core identity and epoch, limits
    /// and status. The same value a socket client gets from the handshake.
    fn server_hello(&self) -> impl Future<Output = Result<ServerHello, WorkbenchError>> + Send;
}
