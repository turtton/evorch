//! A fake backend and helpers shared by the integration tests.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use http::{Request, Response};
use http_body_util::BodyExt;
use std::sync::Mutex;
use tokio::sync::{Semaphore, mpsc};
use tower::ServiceExt;
use workbench_protocol::commands::{CommandDisposition, CommandReply};
use workbench_protocol::envelope::{
    Provenance, ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope,
};
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::events::{
    EventCursor, EventEnvelope, Subscribe, SubscriptionStarted, UiEvent,
};
use workbench_protocol::handshake::{
    ActivityCounts, BuildInfo, Capability, CoreState, CoreStatus, ProtocolVersion, ServerHello,
    WireLimits,
};
use workbench_protocol::ids::{
    ClientId, CoreIdentity, RequestId, SessionId, SubscriptionId, ThreadId,
};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::{InstanceEpoch, Sequence};
use workbench_web::{BackendSubscription, WebSession, WorkbenchBackend};

pub struct Seen {
    pub session: WebSession,
    pub request: RequestEnvelope,
    pub provenance: Provenance,
}

#[derive(Default)]
struct Inner {
    seen: Vec<Seen>,
    subscribes: Vec<(ClientId, Subscribe)>,
    fail_with: Option<WorkbenchError>,
    mismatched_reply: bool,
    subscription: Option<Result<BackendSubscription, WorkbenchError>>,
    status_error: Option<WorkbenchError>,
    protector: Option<fn(serde_json::Value) -> serde_json::Value>,
}

struct Gate {
    permits: Semaphore,
    entered: mpsc::UnboundedSender<()>,
    done: mpsc::UnboundedSender<()>,
}

/// Scripted backend. Requests succeed unless `fail_with` is set.
pub struct FakeBackend {
    inner: Mutex<Inner>,
    /// When set, `handle_request` announces itself on `entered` and then waits
    /// for a permit, so a test can hold requests in flight deterministically.
    gate: Option<Gate>,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            gate: None,
        }
    }

    /// A backend whose requests wait for `release`. The receivers fire when a
    /// request enters the backend and when it has been recorded.
    pub fn gated() -> (
        Self,
        mpsc::UnboundedReceiver<()>,
        mpsc::UnboundedReceiver<()>,
    ) {
        let (entered, entered_rx) = mpsc::unbounded_channel();
        let (done, done_rx) = mpsc::unbounded_channel();
        let backend = Self {
            inner: Mutex::new(Inner::default()),
            gate: Some(Gate {
                permits: Semaphore::new(0),
                entered,
                done,
            }),
        };
        (backend, entered_rx, done_rx)
    }

    pub fn release(&self, permits: usize) {
        self.gate.as_ref().unwrap().permits.add_permits(permits);
    }

    pub fn fail_with(&self, error: WorkbenchError) {
        self.inner.lock().unwrap().fail_with = Some(error);
    }

    pub fn reply_with_wrong_request_id(&self) {
        self.inner.lock().unwrap().mismatched_reply = true;
    }

    pub fn protect_with(&self, protector: fn(serde_json::Value) -> serde_json::Value) {
        self.inner.lock().unwrap().protector = Some(protector);
    }

    pub fn fail_status(&self, error: WorkbenchError) {
        self.inner.lock().unwrap().status_error = Some(error);
    }

    pub fn script_subscription(&self, subscription: Result<BackendSubscription, WorkbenchError>) {
        self.inner.lock().unwrap().subscription = Some(subscription);
    }

    pub fn seen_count(&self) -> usize {
        self.inner.lock().unwrap().seen.len()
    }

    pub fn with_seen<T>(&self, f: impl FnOnce(&[Seen]) -> T) -> T {
        f(&self.inner.lock().unwrap().seen)
    }

    pub fn subscribes(&self) -> Vec<(ClientId, Subscribe)> {
        self.inner.lock().unwrap().subscribes.clone()
    }
}

pub fn core_status() -> CoreStatus {
    CoreStatus {
        state: CoreState::Ready,
        retention_reasons: Vec::new(),
        running: ActivityCounts::default(),
    }
}

pub fn server_hello() -> ServerHello {
    ServerHello {
        protocol_version: ProtocolVersion::CURRENT,
        capabilities: Capability::ALL.to_vec(),
        server_build: BuildInfo {
            name: "evorch-core".to_owned(),
            version: "9.9.9".to_owned(),
            revision: Some("abc123".to_owned()),
        },
        core: CoreIdentity::new("core-test").unwrap(),
        instance_epoch: InstanceEpoch(7),
        limits: WireLimits::default(),
        status: core_status(),
    }
}

impl WorkbenchBackend for FakeBackend {
    fn protect_output(&self, output: serde_json::Value) -> serde_json::Value {
        match self.inner.lock().unwrap().protector {
            Some(protect) => protect(output),
            None => output,
        }
    }

    async fn handle_request(
        &self,
        session: &WebSession,
        request: RequestEnvelope,
        provenance: Provenance,
    ) -> ReplyEnvelope {
        if let Some(gate) = &self.gate {
            let _ = gate.entered.send(());
            gate.permits.acquire().await.unwrap().forget();
        }
        let request_id = request.request_id.clone();
        let body = request.body.clone();
        let (fail, mismatched) = {
            let mut inner = self.inner.lock().unwrap();
            inner.seen.push(Seen {
                session: session.clone(),
                request,
                provenance,
            });
            (inner.fail_with.clone(), inner.mismatched_reply)
        };
        if let Some(gate) = &self.gate {
            let _ = gate.done.send(());
        }
        let request_id = if mismatched {
            RequestId::new("someone-else").unwrap()
        } else {
            request_id
        };
        let result = match fail {
            Some(error) => ReplyResult::Error(error),
            None => ReplyResult::Success(match body {
                RequestBody::Command(_) => ReplyPayload::Command(CommandReply {
                    conversation: None,
                    lane_management: None,
                    lane_close: None,
                    disposition: CommandDisposition::Accepted,
                    submission_id: None,
                    run_id: None,
                    thread_revision: None,
                }),
                RequestBody::Query(Query::GetCoreStatus) => {
                    ReplyPayload::Query(QueryReply::CoreStatus(core_status()))
                }
                _ => ReplyPayload::Ack,
            }),
        };
        ReplyEnvelope { request_id, result }
    }

    async fn subscribe(
        &self,
        client: ClientId,
        subscribe: Subscribe,
    ) -> Result<BackendSubscription, WorkbenchError> {
        let mut inner = self.inner.lock().unwrap();
        inner.subscribes.push((client, subscribe));
        inner
            .subscription
            .take()
            .expect("test did not script a subscription")
    }

    async fn server_hello(&self) -> Result<ServerHello, WorkbenchError> {
        match self.inner.lock().unwrap().status_error.clone() {
            Some(error) => Err(error),
            None => Ok(server_hello()),
        }
    }
}

pub fn cursor(epoch: u64, sequence: u64) -> EventCursor {
    EventCursor {
        core_epoch: InstanceEpoch(epoch),
        sequence: Sequence(sequence),
    }
}

pub fn thread() -> ThreadId {
    ThreadId::new("thr-1").unwrap()
}

pub fn started(high_water: EventCursor) -> SubscriptionStarted {
    SubscriptionStarted {
        subscription_id: SubscriptionId::new("sub-1").unwrap(),
        snapshot_id: None,
        high_water,
        initial_page: None,
    }
}

pub fn event(cursor: EventCursor, event: UiEvent) -> EventEnvelope {
    EventEnvelope {
        subscription_id: SubscriptionId::new("sub-1").unwrap(),
        cursor,
        caused_by_request_id: None,
        event,
    }
}

pub fn status_event(cursor: EventCursor) -> EventEnvelope {
    event(
        cursor,
        UiEvent::CoreStatusChanged {
            status: core_status(),
        },
    )
}

/// A subscription whose events the test feeds through the returned sender.
pub fn subscription(high_water: EventCursor) -> (BackendSubscription, mpsc::Sender<EventEnvelope>) {
    let (tx, rx) = mpsc::channel(256);
    (
        BackendSubscription {
            started: started(high_water),
            events: rx,
        },
        tx,
    )
}

pub const HOST: &str = "127.0.0.1:7878";

pub fn query_request(request_id: &str) -> serde_json::Value {
    serde_json::json!({
        "request_id": request_id,
        "session_id": "sess-1",
        "kind": "query",
        "payload": {"type": "get_core_status"}
    })
}

pub fn command_request(request_id: &str) -> serde_json::Value {
    serde_json::json!({
        "request_id": request_id,
        "session_id": "sess-1",
        "kind": "command",
        "payload": {"type": "stop_chat", "payload": {"thread": "thr-1"}}
    })
}

pub fn json_post() -> http::request::Builder {
    Request::builder()
        .method("POST")
        .uri("/api/v1/request")
        .header("host", HOST)
        .header("content-type", "application/json")
}

pub fn post_json(body: &serde_json::Value) -> Request<Body> {
    json_post()
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

pub fn get(uri: &str) -> http::request::Builder {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", HOST)
}

pub async fn send(router: &Router, request: Request<Body>) -> Response<Body> {
    router.clone().oneshot(request).await.unwrap()
}

pub async fn body_bytes(response: Response<Body>) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

pub async fn body_json(response: Response<Body>) -> serde_json::Value {
    serde_json::from_slice(&body_bytes(response).await).unwrap()
}

pub fn session_id() -> SessionId {
    SessionId::new("sess-1").unwrap()
}

/// One SSE message: `event`/`id`/`data` fields, or a comment.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SseMessage {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: Option<String>,
    pub comment: Option<String>,
}

impl SseMessage {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(self.data.as_deref().unwrap()).unwrap()
    }
}

/// Incremental SSE parser over a response body.
pub struct SseReader {
    body: Body,
    buffer: String,
}

impl SseReader {
    pub fn new(response: Response<Body>) -> Self {
        Self {
            body: response.into_body(),
            buffer: String::new(),
        }
    }

    /// The next message, or `None` once the stream has ended.
    pub async fn next(&mut self) -> Option<SseMessage> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let raw: String = self.buffer.drain(..end + 2).collect();
                let mut message = SseMessage::default();
                for line in raw.lines() {
                    if let Some(comment) = line.strip_prefix(':') {
                        message.comment = Some(comment.trim().to_owned());
                    } else if let Some(value) = line.strip_prefix("id:") {
                        message.id = Some(value.trim().to_owned());
                    } else if let Some(value) = line.strip_prefix("event:") {
                        message.event = Some(value.trim().to_owned());
                    } else if let Some(value) = line.strip_prefix("data:") {
                        message.data = Some(value.trim().to_owned());
                    }
                }
                return Some(message);
            }
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buffer.push_str(std::str::from_utf8(&data).unwrap());
                    }
                }
                Some(Err(error)) => panic!("sse body error: {error}"),
                None => return None,
            }
        }
    }

    /// Next message that is not a keepalive comment.
    pub async fn next_event(&mut self) -> Option<SseMessage> {
        loop {
            let message = self.next().await?;
            if message.comment.is_none() {
                return Some(message);
            }
        }
    }
}
