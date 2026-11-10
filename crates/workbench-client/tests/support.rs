//! Shared helpers for the client tests. Nothing here waits on time.
#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use workbench_client::fake::{FakeServer, FakeTransport};
use workbench_client::{ClientConfig, Subscription, WorkbenchClient};
use workbench_protocol::commands::*;
use workbench_protocol::envelope::*;
use workbench_protocol::events::*;
use workbench_protocol::handshake::*;
use workbench_protocol::ids::*;
use workbench_protocol::queries::*;
use workbench_protocol::views::*;
use workbench_protocol::wire::*;

pub fn build() -> BuildInfo {
    BuildInfo {
        name: "test".into(),
        version: "0.0.0".into(),
        revision: None,
    }
}

pub fn config() -> ClientConfig {
    ClientConfig::new(ClientId::new("client-1").unwrap(), build())
}

pub fn status() -> CoreStatus {
    CoreStatus {
        state: CoreState::Ready,
        retention_reasons: vec![],
        running: ActivityCounts::default(),
    }
}

pub fn server_hello(limits: WireLimits) -> ServerHello {
    ServerHello {
        protocol_version: ProtocolVersion::CURRENT,
        capabilities: Capability::ALL.to_vec(),
        server_build: build(),
        core: CoreIdentity::new("core-abc").unwrap(),
        instance_epoch: InstanceEpoch(7),
        limits,
        status: status(),
    }
}

/// A connected client and its fake core, handshake done.
pub async fn connected() -> (WorkbenchClient, FakeServer) {
    connected_with(server_hello(WireLimits::default())).await
}

pub async fn connected_with(hello: ServerHello) -> (WorkbenchClient, FakeServer) {
    let (transport, mut server) = FakeTransport::pair();
    let (client, _) = tokio::join!(
        async { WorkbenchClient::connect(transport, config()).await.unwrap() },
        server.handshake(&hello),
    );
    (client, server)
}

pub fn limits(max_in_flight: u32) -> WireLimits {
    WireLimits {
        max_in_flight_requests: max_in_flight,
        ..WireLimits::default()
    }
}

/// Small limits so size-related tests need only kilobytes.
pub fn small_limits() -> WireLimits {
    WireLimits {
        max_frame_bytes: 8192,
        max_body_chunk_bytes: 512,
        max_in_flight_requests: 8,
        subscription_queue_events: 1024,
        subscription_queue_bytes: 8192,
    }
}

pub fn thread() -> ThreadId {
    ThreadId::new("thr-1").unwrap()
}

pub fn stop_chat(thread: &str) -> Command {
    Command::StopChat(ThreadTarget {
        thread: ThreadId::new(thread).unwrap(),
    })
}

pub fn command_reply() -> CommandReply {
    CommandReply {
        conversation: None,
        lane_management: None,
        disposition: CommandDisposition::Accepted,
        submission_id: None,
        run_id: None,
        thread_revision: None,
    }
}

pub fn ok(payload: ReplyPayload) -> ReplyResult {
    ReplyResult::Success(payload)
}

pub fn status_query() -> Query {
    Query::GetCoreStatus
}

/// Poll a future once and keep it. A client request is queued by its first
/// poll, so this puts the request in flight without waiting for the reply.
pub async fn poll_once<F: Future + Unpin>(future: &mut F) {
    std::future::poll_fn(|cx| {
        let _ = Pin::new(&mut *future).poll(cx);
        Poll::Ready(())
    })
    .await;
}

pub fn cursor(epoch: u64, sequence: u64) -> EventCursor {
    EventCursor {
        core_epoch: InstanceEpoch(epoch),
        sequence: Sequence(sequence),
    }
}

pub fn subscribe_all() -> Subscribe {
    Subscribe {
        scope: SubscriptionScope::All,
        after: None,
    }
}

pub fn started(id: &str, high_water: EventCursor) -> SubscriptionStarted {
    SubscriptionStarted {
        subscription_id: SubscriptionId::new(id).unwrap(),
        snapshot_id: None,
        high_water,
        initial_page: None,
    }
}

/// Subscribe through the fake core, which answers with `high_water` as the snapshot boundary.
pub async fn subscribe(
    client: &WorkbenchClient,
    server: &mut FakeServer,
    id: &str,
    high_water: EventCursor,
) -> Subscription {
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(subscribe_all()).await }
    });
    let request = server.recv_request().await.expect("subscribe request");
    assert!(matches!(
        request.body,
        RequestBody::Control(Control::Subscribe(_))
    ));
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(started(id, high_water))),
    );
    call.await.unwrap().expect("subscription")
}

pub fn event(id: &str, cursor: EventCursor, event: UiEvent) -> EventEnvelope {
    EventEnvelope {
        subscription_id: SubscriptionId::new(id).unwrap(),
        cursor,
        caused_by_request_id: None,
        event,
    }
}

pub fn status_event(id: &str, cursor: EventCursor) -> EventEnvelope {
    event(id, cursor, UiEvent::CoreStatusChanged { status: status() })
}

pub fn thread_changed(id: &str, cursor: EventCursor, title: &str) -> EventEnvelope {
    event(
        id,
        cursor,
        UiEvent::ThreadChanged {
            thread: ThreadSummary {
                id: thread(),
                project: ProjectId::new("proj-1").unwrap(),
                title: title.into(),
                state: ThreadState::Idle,
                lead_thread: None,
                revision: Revision(1),
                updated_at: UnixMillis(0),
            },
        },
    )
}

/// Round-trip a ping so the reader has processed everything the core sent before it.
pub async fn barrier(client: &WorkbenchClient, server: &mut FakeServer) {
    let ping = tokio::spawn({
        let client = client.clone();
        async move { client.ping(Nonce(99)).await }
    });
    let request = server.recv_request().await.expect("ping");
    assert!(
        matches!(request.body, RequestBody::Control(Control::Ping { .. })),
        "the next request must be the ping, got {:?}",
        request.body
    );
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Pong { nonce: Nonce(99) }),
    );
    ping.await.unwrap().expect("pong");
}

/// Ping behind events the core has already sent, and collect the unsubscribe
/// the client sends on its own when it stops a subscription. The two requests
/// may arrive in either order; the pong, sent last, proves every earlier
/// event has been routed.
pub async fn ping_and_expect_unsubscribe(
    client: &WorkbenchClient,
    server: &mut FakeServer,
    subscription: &SubscriptionId,
) {
    let ping = tokio::spawn({
        let client = client.clone();
        async move { client.ping(Nonce(1)).await }
    });
    let (mut pinged, mut unsubscribed) = (None, false);
    while pinged.is_none() || !unsubscribed {
        let request = server.recv_request().await.expect("client request");
        match request.body {
            RequestBody::Control(Control::Ping { .. }) => pinged = Some(request.request_id),
            RequestBody::Control(Control::Unsubscribe { subscription_id }) => {
                assert_eq!(&subscription_id, subscription);
                unsubscribed = true;
            }
            other => panic!("unexpected request {other:?}"),
        }
    }
    server.reply(&pinged.unwrap(), ok(ReplyPayload::Pong { nonce: Nonce(1) }));
    ping.await.unwrap().expect("pong");
}

/// An event whose size is chosen by the caller, within the inline message bound.
pub fn big_event(id: &str, cursor: EventCursor, bytes: usize) -> EventEnvelope {
    event(
        id,
        cursor,
        UiEvent::MessageCommitted {
            message: CommittedMessage {
                thread: thread(),
                run: RunId::new("run-1").unwrap(),
                index: Sequence(cursor.sequence.get()),
                role: MessageRole::Assistant,
                content: MessageContent::Inline {
                    text: "x".repeat(bytes),
                },
            },
        },
    )
}

pub fn answered_question(answer: String) -> QuestionView {
    let run = RunId::new("run-1").unwrap();
    QuestionView {
        id: QuestionId::new("q-1").unwrap(),
        requester: QuestionRequester {
            run: run.clone(),
            thread: thread(),
            root_run: run.clone(),
        },
        target: QuestionTarget::User,
        management_revision: Revision(1),
        routing_revision: Revision(1),
        blocking: true,
        title: "Proceed?".into(),
        options: vec![],
        answer: Some(QuestionAnswer {
            text: answer,
            answered_by: Some(Provenance::human(
                Surface::Web,
                ClientId::new("client-2").unwrap(),
            )),
        }),
        revision: Revision(2),
        raised_by: Provenance::agent(run),
    }
}
