//! Event multiplexing, ordering around the snapshot boundary and slow subscribers.

mod support;

use support::*;
use workbench_client::{ClientError, SubscriptionEnd, TransportError};
use workbench_protocol::envelope::*;
use workbench_protocol::events::*;
use workbench_protocol::handshake::WireLimits;
use workbench_protocol::ids::*;

fn title(envelope: &EventEnvelope) -> &str {
    match &envelope.event {
        UiEvent::ThreadChanged { thread } => &thread.title,
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
        UiEvent::ResyncRequired {
            reason: ResyncReason::EpochChanged
        }
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
        UiEvent::ResyncRequired {
            reason: ResyncReason::SlowSubscriber
        }
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
    let big = "x".repeat(3000);
    for sequence in 1..=5 {
        server.event(thread_changed("sub-1", cursor(7, sequence), &big));
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
        UiEvent::ResyncRequired {
            reason: ResyncReason::SlowSubscriber
        }
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
        UiEvent::ResyncRequired {
            reason: ResyncReason::CursorTooOld,
        },
    ));
    server.event(thread_changed("sub-1", cursor(7, 2), "after the end"));

    let last = sub.next().await.unwrap();
    assert_eq!(
        last.event,
        UiEvent::ResyncRequired {
            reason: ResyncReason::CursorTooOld
        }
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
