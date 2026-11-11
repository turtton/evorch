//! Event multiplexing, ordering around the snapshot boundary and slow subscribers.

mod support;

use support::*;
use workbench_client::{ClientError, SubscriptionEnd, TransportError};
use workbench_protocol::envelope::*;
use workbench_protocol::events::*;
use workbench_protocol::handshake::WireLimits;
use workbench_protocol::ids::*;
use workbench_protocol::namespaces::{ProjectionEvent, QuestionsEvent, WorkspaceEvent};

fn title(envelope: &EventEnvelope) -> &str {
    match &envelope.event {
        UiEvent::Workspace(WorkspaceEvent::ThreadChanged { thread }) => &thread.title,
        other => panic!("not a thread event: {other:?}"),
    }
}

#[tokio::test]
async fn events_at_or_before_the_snapshot_boundary_and_repeats_are_dropped() {
    let (client, mut server) = connected().await;
    let mut sub = subscribe(&client, &mut server, "sub-1", cursor(7, 40)).await;
    assert_eq!(sub.started().high_water, cursor(7, 40));

    for (sequence, label) in [
        (39, "old"),
        (40, "boundary"),
        (41, "first"),
        (41, "repeat"),
        (43, "second"),
    ] {
        server.event(thread_changed("sub-1", cursor(7, sequence), label));
    }
    server.close();

    let mut seen = Vec::new();
    while let Some(envelope) = sub.next().await {
        seen.push(title(&envelope).to_owned());
    }
    assert_eq!(seen, ["first", "second"]);
    assert_eq!(sub.end(), Some(SubscriptionEnd::Disconnected));
}

#[tokio::test]
async fn an_event_from_another_epoch_stops_the_stream_with_resync_required() {
    let (client, mut server) = connected().await;
    let mut sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    server.event(thread_changed("sub-1", cursor(7, 1), "before"));
    server.event(thread_changed("sub-1", cursor(8, 1), "restarted core"));
    server.event(thread_changed("sub-1", cursor(7, 2), "ignored"));

    assert_eq!(title(&sub.next().await.unwrap()), "before");
    let last = sub.next().await.unwrap();
    assert_eq!(
        last.event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::EpochChanged
        })
    );
    assert_eq!(
        last.cursor,
        cursor(7, 1),
        "positioned at the last event delivered"
    );
    assert_eq!(sub.next().await, None);
    assert_eq!(sub.end(), Some(SubscriptionEnd::EpochChanged));

    // The client tells the core to stop sending.
    let request = server.recv_request().await.unwrap();
    assert_eq!(
        request.body,
        RequestBody::Control(Control::Unsubscribe {
            subscription_id: sub.id().clone()
        })
    );
}

#[tokio::test]
async fn a_slow_subscriber_gets_resync_required_and_is_stopped() {
    let mut hello = server_hello(WireLimits::default());
    hello.limits.subscription_queue_events = 2;
    let (client, mut server) = connected_with(hello).await;
    let mut sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;

    // Nobody is reading `sub`: the third event does not fit.
    for sequence in 1..=4 {
        server.event(thread_changed(
            "sub-1",
            cursor(7, sequence),
            &format!("e{sequence}"),
        ));
    }
    // The pong arrives after the events, so all four have been routed.
    ping_and_expect_unsubscribe(&client, &mut server, sub.id()).await;

    assert_eq!(title(&sub.next().await.unwrap()), "e1");
    assert_eq!(title(&sub.next().await.unwrap()), "e2");
    let last = sub.next().await.unwrap();
    assert_eq!(
        last.event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::SlowSubscriber
        })
    );
    assert_eq!(sub.next().await, None);
    assert_eq!(sub.end(), Some(SubscriptionEnd::SlowSubscriber));
    // Stopping one subscriber does not disturb the connection.
    barrier(&client, &mut server).await;
}

#[tokio::test]
async fn a_subscriber_over_its_byte_budget_is_slow_too() {
    let (client, mut server) = connected_with(server_hello(small_limits())).await;
    let mut sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    for sequence in 1..=5 {
        server.event(big_event("sub-1", cursor(7, sequence), 3000));
    }
    ping_and_expect_unsubscribe(&client, &mut server, sub.id()).await;

    let mut delivered = 0;
    let mut last = None;
    while let Some(envelope) = sub.next().await {
        if !envelope.event.ends_subscription() {
            delivered += 1;
        }
        last = Some(envelope);
    }
    assert_eq!(delivered, 2, "8192 bytes hold two 3000-byte events");
    assert_eq!(
        last.unwrap().event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::SlowSubscriber
        })
    );
}

#[tokio::test]
async fn reading_events_frees_budget_so_a_keeping_up_subscriber_is_never_slow() {
    let mut hello = server_hello(WireLimits::default());
    hello.limits.subscription_queue_events = 2;
    let (client, mut server) = connected_with(hello).await;
    let mut sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    for sequence in 1..=50 {
        server.event(thread_changed(
            "sub-1",
            cursor(7, sequence),
            &format!("e{sequence}"),
        ));
        assert_eq!(title(&sub.next().await.unwrap()), format!("e{sequence}"));
    }
    assert_eq!(sub.end(), None);
}

#[tokio::test]
async fn a_resync_from_the_core_is_delivered_and_ends_the_subscription() {
    let (client, mut server) = connected().await;
    let mut sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    server.event(event(
        "sub-1",
        cursor(7, 1),
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::CursorTooOld,
        }),
    ));
    server.event(thread_changed("sub-1", cursor(7, 2), "after the end"));

    let last = sub.next().await.unwrap();
    assert_eq!(
        last.event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::CursorTooOld
        })
    );
    assert_eq!(sub.next().await, None);
    assert_eq!(sub.end(), Some(SubscriptionEnd::EndedByCore));
    barrier(&client, &mut server).await; // later events for it are ignored, not violations
}

#[tokio::test]
async fn two_subscriptions_receive_only_their_own_events() {
    let (client, mut server) = connected().await;
    let mut one = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    let mut two = subscribe(&client, &mut server, "sub-2", cursor(7, 10)).await;

    server.event(thread_changed("sub-2", cursor(7, 11), "for two"));
    server.event(thread_changed("sub-1", cursor(7, 1), "for one"));
    server.event(thread_changed(
        "no-such-subscription",
        cursor(7, 2),
        "for nobody",
    ));
    server.event(thread_changed("sub-1", cursor(7, 2), "for one again"));

    assert_eq!(title(&two.next().await.unwrap()), "for two");
    assert_eq!(title(&one.next().await.unwrap()), "for one");
    assert_eq!(title(&one.next().await.unwrap()), "for one again");
    barrier(&client, &mut server).await;
}

#[tokio::test]
async fn unsubscribe_is_confirmed_by_the_core_and_later_events_are_ignored() {
    let (client, mut server) = connected().await;
    let sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    let call = tokio::spawn(async move { sub.unsubscribe().await });
    let request = server.recv_request().await.unwrap();
    assert_eq!(
        request.body,
        RequestBody::Control(Control::Unsubscribe {
            subscription_id: SubscriptionId::new("sub-1").unwrap()
        })
    );
    server.event(thread_changed("sub-1", cursor(7, 1), "in flight"));
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Unsubscribed {
            subscription_id: SubscriptionId::new("sub-1").unwrap(),
        }),
    );
    call.await.unwrap().unwrap();
    barrier(&client, &mut server).await;
}

#[tokio::test]
async fn dropping_a_subscription_tells_the_core() {
    let (client, mut server) = connected().await;
    let sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    drop(sub);
    let request = server.recv_request().await.unwrap();
    assert_eq!(
        request.body,
        RequestBody::Control(Control::Unsubscribe {
            subscription_id: SubscriptionId::new("sub-1").unwrap()
        })
    );
    // Its reply has no waiter and must not be mistaken for an unknown request.
    server.reply(&request.request_id, ok(ReplyPayload::Ack));
    barrier(&client, &mut server).await;
}

#[tokio::test]
async fn losing_the_connection_ends_every_subscription() {
    let (client, mut server) = connected().await;
    let mut one = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    let mut two = subscribe(&client, &mut server, "sub-2", cursor(7, 0)).await;
    server.close();
    assert_eq!(one.next().await, None);
    assert_eq!(two.next().await, None);
    assert_eq!(one.end(), Some(SubscriptionEnd::Disconnected));
    assert_eq!(two.end(), Some(SubscriptionEnd::Disconnected));
    assert_eq!(
        client.subscribe(subscribe_all()).await.err(),
        Some(ClientError::Transport(TransportError::Disconnected))
    );
}

#[tokio::test]
async fn a_reused_subscription_id_is_a_protocol_violation() {
    let (client, mut server) = connected().await;
    let _live = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(subscribe_all()).await }
    });
    let request = server.recv_request().await.unwrap();
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(started(
            "sub-1",
            cursor(7, 0),
        ))),
    );
    assert!(matches!(
        call.await.unwrap().err(),
        Some(ClientError::Transport(TransportError::ProtocolViolation(_)))
    ));
}

#[tokio::test]
async fn the_subscribe_reply_carries_the_snapshot() {
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(subscribe_all()).await }
    });
    let request = server.recv_request().await.unwrap();
    let mut started = started("sub-1", cursor(7, 3));
    started.snapshot_id = Some(SnapshotId::new("snap-1").unwrap());
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(started.clone())),
    );
    let sub = call.await.unwrap().unwrap();
    assert_eq!(sub.started(), &started);
}

// --- ownership of a subscription the caller never received -------------------------

async fn expect_unsubscribe(server: &mut workbench_client::fake::FakeServer, id: &str) {
    let request = server.recv_request().await.unwrap();
    assert_eq!(
        request.body,
        RequestBody::Control(Control::Unsubscribe {
            subscription_id: SubscriptionId::new(id).unwrap()
        })
    );
    server.reply(&request.request_id, ok(ReplyPayload::Ack));
}

#[tokio::test]
async fn a_subscribe_future_dropped_before_the_reply_does_not_leave_a_subscription() {
    let (client, mut server) = connected().await;
    let mut abandoned = Box::pin(client.subscribe(subscribe_all()));
    poll_once(&mut abandoned).await;
    let request = server.recv_request().await.unwrap();
    drop(abandoned);

    // The reply arrives for a caller that is gone: the core is told to stop.
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(started(
            "sub-1",
            cursor(7, 0),
        ))),
    );
    expect_unsubscribe(&mut server, "sub-1").await;

    // Registration is gone too: the same id can be used again.
    let mut again = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    server.event(thread_changed("sub-1", cursor(7, 1), "live"));
    assert_eq!(title(&again.next().await.unwrap()), "live");
}

#[tokio::test]
async fn a_reply_the_caller_never_collected_does_not_leave_a_subscription() {
    let (client, mut server) = connected().await;
    let mut pending = Box::pin(client.subscribe(subscribe_all()));
    poll_once(&mut pending).await;
    let request = server.recv_request().await.unwrap();
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(started(
            "sub-1",
            cursor(7, 0),
        ))),
    );
    // The pong follows the reply, so the reply has been handled and the
    // subscription registered, but the dropped future never collects it.
    barrier(&client, &mut server).await;
    drop(pending);

    expect_unsubscribe(&mut server, "sub-1").await;
    let mut again = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    server.event(thread_changed("sub-1", cursor(7, 1), "live"));
    assert_eq!(title(&again.next().await.unwrap()), "live");
}

#[tokio::test]
async fn an_unsubscribe_after_a_close_sends_nothing_and_does_not_panic() {
    let (client, mut server) = connected().await;
    let sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    server.close();
    client.closed().await;
    drop(sub);
}

// --- resuming with a cursor ----------------------------------------------------------

fn resume_from(sequence: u64) -> Subscribe {
    Subscribe {
        scope: SubscriptionScope::All,
        after: Some(cursor(7, sequence)),
    }
}

#[tokio::test]
async fn resuming_replays_events_after_the_cursor_without_a_snapshot() {
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(resume_from(41)).await }
    });
    let request = server.recv_request().await.unwrap();
    assert_eq!(
        request.body,
        RequestBody::Control(Control::Subscribe(resume_from(41)))
    );
    // Replay: no snapshot, and the stream continues from the requested cursor.
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(started(
            "sub-1",
            cursor(7, 41),
        ))),
    );
    let mut sub = call.await.unwrap().unwrap();
    assert_eq!(sub.started().snapshot_id, None);

    for (sequence, label) in [(41, "already seen"), (42, "replayed"), (43, "live")] {
        server.event(thread_changed("sub-1", cursor(7, sequence), label));
    }
    assert_eq!(title(&sub.next().await.unwrap()), "replayed");
    assert_eq!(title(&sub.next().await.unwrap()), "live");
}

#[tokio::test]
async fn a_cursor_the_core_cannot_replay_falls_back_to_a_snapshot_in_the_new_epoch() {
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(resume_from(41)).await }
    });
    let request = server.recv_request().await.unwrap();
    let mut fallback = started("sub-1", cursor(8, 5));
    fallback.snapshot_id = Some(SnapshotId::new("snap-1").unwrap());
    fallback.initial_page = Some(workbench_protocol::views::Page {
        items: vec![SnapshotItem::CoreStatus(status())],
        next_cursor: None,
        snapshot_revision: workbench_protocol::wire::Revision(3),
    });
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(fallback)),
    );
    let mut sub = call.await.unwrap().unwrap();
    assert!(sub.started().snapshot_id.is_some());
    assert_eq!(sub.started().initial_page.as_ref().unwrap().items.len(), 1);

    // The boundary is in epoch 8: its old events are covered by the snapshot,
    // later ones are delivered, and an event of the previous epoch is not.
    server.event(thread_changed("sub-1", cursor(8, 5), "covered by snapshot"));
    server.event(thread_changed("sub-1", cursor(8, 6), "after snapshot"));
    server.event(thread_changed("sub-1", cursor(7, 50), "stale epoch"));
    assert_eq!(title(&sub.next().await.unwrap()), "after snapshot");
    assert_eq!(
        sub.next().await.unwrap().event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::EpochChanged
        })
    );
    assert_eq!(sub.next().await, None);
}

// --- received events are validated -------------------------------------------------------

#[tokio::test]
async fn an_event_breaking_its_bounds_closes_the_connection() {
    let (client, mut server) = connected().await;
    let mut sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    server.event(thread_changed("sub-1", cursor(7, 1), &"t".repeat(2000)));
    assert!(matches!(
        client.closed().await,
        TransportError::ProtocolViolation(_)
    ));
    assert_eq!(sub.next().await, None);
    assert_eq!(sub.end(), Some(SubscriptionEnd::Disconnected));
}

#[tokio::test]
async fn a_snapshot_page_over_the_page_limit_fails_the_subscribe() {
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(subscribe_all()).await }
    });
    let request = server.recv_request().await.unwrap();
    let mut oversized = started("sub-1", cursor(7, 0));
    oversized.initial_page = Some(workbench_protocol::views::Page {
        items: vec![SnapshotItem::CoreStatus(status()); 201],
        next_cursor: None,
        snapshot_revision: workbench_protocol::wire::Revision(1),
    });
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(oversized)),
    );
    assert!(matches!(
        call.await.unwrap().err(),
        Some(ClientError::Transport(TransportError::ProtocolViolation(_)))
    ));
}

// --- answers on the snapshot and event paths --------------------------------------------------

#[tokio::test]
async fn a_question_answer_over_256_kib_is_refused_in_a_snapshot_and_in_an_event() {
    // The limit itself passes on both paths.
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(subscribe_all()).await }
    });
    let request = server.recv_request().await.unwrap();
    let mut snapshot = started("sub-1", cursor(7, 0));
    snapshot.initial_page = Some(workbench_protocol::views::Page {
        items: vec![SnapshotItem::Question(answered_question(
            "x".repeat(256 * 1024),
        ))],
        next_cursor: None,
        snapshot_revision: workbench_protocol::wire::Revision(1),
    });
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(snapshot)),
    );
    let mut sub = call.await.unwrap().unwrap();
    server.event(event(
        "sub-1",
        cursor(7, 1),
        UiEvent::Questions(QuestionsEvent::QuestionChanged {
            question: answered_question("x".repeat(256 * 1024)),
        }),
    ));
    assert!(matches!(
        sub.next().await.unwrap().event,
        UiEvent::Questions(QuestionsEvent::QuestionChanged { .. })
    ));

    // One byte more is refused: on the snapshot...
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(subscribe_all()).await }
    });
    let request = server.recv_request().await.unwrap();
    let mut snapshot = started("sub-1", cursor(7, 0));
    snapshot.initial_page = Some(workbench_protocol::views::Page {
        items: vec![SnapshotItem::Question(answered_question(
            "x".repeat(256 * 1024 + 1),
        ))],
        next_cursor: None,
        snapshot_revision: workbench_protocol::wire::Revision(1),
    });
    server.reply(
        &request.request_id,
        ok(ReplyPayload::SubscriptionStarted(snapshot)),
    );
    assert!(matches!(
        call.await.unwrap().err(),
        Some(ClientError::Transport(TransportError::ProtocolViolation(_)))
    ));

    // ...and on an event.
    let (client, mut server) = connected().await;
    let mut sub = subscribe(&client, &mut server, "sub-1", cursor(7, 0)).await;
    server.event(event(
        "sub-1",
        cursor(7, 1),
        UiEvent::Questions(QuestionsEvent::QuestionChanged {
            question: answered_question("x".repeat(256 * 1024 + 1)),
        }),
    ));
    assert!(matches!(
        client.closed().await,
        TransportError::ProtocolViolation(_)
    ));
    assert_eq!(sub.next().await, None);
}

// --- a finished subscription must not release its successor ---------------------------------------
//
// A conforming core never reuses a subscription id within a connection. These
// tests cover defence against a non-conforming peer that does, so a stale
// handle can never stop a live subscription.

/// Subscribe "sub-1", let the core end it, then have a non-conforming core reuse "sub-1"
/// for a new subscription, and return both.
async fn ended_then_reused(
    client: &workbench_client::WorkbenchClient,
    server: &mut workbench_client::fake::FakeServer,
) -> (
    workbench_client::Subscription,
    workbench_client::Subscription,
) {
    let mut old = subscribe(client, server, "sub-1", cursor(7, 0)).await;
    server.event(event(
        "sub-1",
        cursor(7, 1),
        UiEvent::Projection(ProjectionEvent::SubscriptionClosed {
            reason: CloseReason::ScopeRemoved,
        }),
    ));
    assert!(matches!(
        old.next().await.unwrap().event,
        UiEvent::Projection(ProjectionEvent::SubscriptionClosed { .. })
    ));
    assert_eq!(old.next().await, None);
    assert_eq!(old.end(), Some(SubscriptionEnd::EndedByCore));
    let new = subscribe(client, server, "sub-1", cursor(7, 1)).await;
    (old, new)
}

#[tokio::test]
async fn dropping_a_finished_subscription_leaves_one_that_reused_its_id() {
    let (client, mut server) = connected().await;
    let (old, mut new) = ended_then_reused(&client, &mut server).await;
    drop(old);
    server.event(thread_changed("sub-1", cursor(7, 2), "for the new one"));
    assert_eq!(title(&new.next().await.unwrap()), "for the new one");
    // barrier insists the next request the core sees is its ping, so no
    // Unsubscribe for the reused id was sent.
    barrier(&client, &mut server).await;
    assert_eq!(new.end(), None);
}

#[tokio::test]
async fn unsubscribing_a_finished_subscription_leaves_one_that_reused_its_id() {
    let (client, mut server) = connected().await;
    let (old, mut new) = ended_then_reused(&client, &mut server).await;
    old.unsubscribe()
        .await
        .expect("already over: nothing to ask the core");
    server.event(thread_changed("sub-1", cursor(7, 2), "for the new one"));
    assert_eq!(title(&new.next().await.unwrap()), "for the new one");
    barrier(&client, &mut server).await;
    assert_eq!(new.end(), None);
}

#[tokio::test]
async fn the_current_subscription_is_still_released_by_drop_and_by_unsubscribe() {
    let (client, mut server) = connected().await;
    let (old, new) = ended_then_reused(&client, &mut server).await;
    drop(old);
    drop(new);
    expect_unsubscribe(&mut server, "sub-1").await;

    let sub = subscribe(&client, &mut server, "sub-1", cursor(7, 5)).await;
    let call = tokio::spawn(async move { sub.unsubscribe().await });
    let request = server.recv_request().await.unwrap();
    assert_eq!(
        request.body,
        RequestBody::Control(Control::Unsubscribe {
            subscription_id: SubscriptionId::new("sub-1").unwrap()
        })
    );
    server.reply(&request.request_id, ok(ReplyPayload::Ack));
    call.await.unwrap().unwrap();
}
