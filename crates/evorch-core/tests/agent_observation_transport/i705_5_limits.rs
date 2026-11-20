//! #705 item 5: races, the 30 seconds, slow readers and the eight shared
//! places, on the real sockets.
//!
//! Time. Tests that need a window use the paused tokio clock, and a paused
//! runtime jumps to its next timer whenever it is idle, including while it
//! waits for a socket. So a paused test is built so that the timer it wants is
//! the only one that can be pending when it waits: an unbound channel's 30
//! seconds, a withheld ACK's 5 seconds, or a registry window with no timer at
//! all (advanced explicitly). Every other test runs on the real clock and
//! finishes in milliseconds because nothing in it waits for a window.
use crate::harness::*;
use std::time::Duration;
use workbench_protocol::{
    errors::ErrorCode,
    namespaces::browser::{
        data::{self, MAX_FRAME_SUBSCRIPTIONS, MAX_UNBOUND_CONNECTIONS, NO_PROGRESS_MS},
        observation::*,
    },
};

fn scope_with_page(core: &Core) -> ObservationScope {
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "first", [1, 2, 3, 255]))
    );
    scope
}

fn bound(result: &Result<ObservationResult, ErrorCode>) -> bool {
    match result {
        Ok(ObservationResult::Bound {}) => true,
        Err(ErrorCode::PermissionDenied) => false,
        other => panic!("neither bound nor refused: {other:?}"),
    }
}

/// Two control connections of one client bind one token at once: exactly one
/// wins, the other channel stays unbound. Racing the bind against an
/// unsubscribe ends with no subscription and no open stream, whoever wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn i705_5_bind_races_have_one_winner_and_leave_no_stream_after_unsubscribe() {
    let mut core = Core::start().await;
    let scope = scope_with_page(&core);
    let first = core.connect("native-a").await;
    let second = core.connect("native-a").await;

    for round in 0..16 {
        // two binds of one token
        let sub = first.subscribe(&scope).await;
        let (one, two) = (core.data().await, core.data().await);
        let (r1, r2) = tokio::join!(
            first.bind(&one.channel, &sub.subscription, &sub.token),
            second.bind(&two.channel, &sub.subscription, &sub.token)
        );
        assert_ne!(bound(&r1), bound(&r2), "round {round}: exactly one winner");
        let (mut winner, loser) = if bound(&r1) { (one, two) } else { (two, one) };
        assert_eq!(winner.frame().await.0.sequence, 1, "round {round}");
        // the loser's channel was never bound: it is closed by its peer
        let loser_conn = loser.conn;
        drop(loser);
        core.wait_ended(loser_conn).await;
        first.unsubscribe(&sub.subscription).await.unwrap();
        winner.expect_closed().await;
        core.wait_ended(winner.conn).await;

        // bind against unsubscribe
        let sub = first.subscribe(&scope).await;
        let mut one = core.data().await;
        let (bind, unsubscribe) = tokio::join!(
            first.bind(&one.channel, &sub.subscription, &sub.token),
            second.unsubscribe(&sub.subscription)
        );
        assert_eq!(unsubscribe.unwrap(), ObservationResult::Unsubscribed {});
        if bound(&bind) {
            // the stream was bound before it was revoked: it ends with a
            // reason, after at most the frame that was already in flight
            match one.item().await {
                Item::Frame(..) => one.expect_closed().await,
                Item::Error(data::ErrorCode::Closed) => {
                    assert!(matches!(one.item().await, Item::Eof));
                }
                other => panic!("round {round}: {other:?}"),
            }
        }
        let conn = one.conn;
        drop(one);
        core.wait_ended(conn).await;
        assert_eq!(
            first.subscription(&sub.subscription).await.unwrap_err(),
            ErrorCode::PermissionDenied,
            "round {round}"
        );
    }
    // no place leaked in any round
    for _ in 0..MAX_FRAME_SUBSCRIPTIONS {
        first.subscribe(&scope).await;
    }
    assert_eq!(
        first.subscribe_raw(&scope).await.unwrap(),
        ObservationResult::State(ObservationState::Capacity)
    );
}

/// Eight data connections may wait for a binding, the ninth is refused with
/// Busy, and every unbound one is closed with Expired when its 30 seconds are
/// up, never earlier. The wait is virtual.
#[tokio::test]
async fn i705_5_unbound_channels_are_capped_at_eight_and_expire_with_the_bind_window() {
    let mut core = Core::start().await;
    scope_with_page(&core);
    let mut waiting = Vec::new();
    for _ in 0..MAX_UNBOUND_CONNECTIONS {
        waiting.push(core.data().await);
    }
    assert_eq!(core.data_refused().await, data::ErrorCode::Busy);
    // only now stop the clock: with timers pending, every wait for a socket
    // would jump to them
    pause();
    for data in &mut waiting {
        data.expect_expired_after_the_window().await;
    }
    for data in &waiting {
        core.wait_ended(data.conn).await;
    }
    // the places came back
    let again = core.data().await;
    assert_ne!(again.channel, waiting[0].channel);
}

/// A subscription nobody bound ends with the same 30 seconds and gives its
/// place back; a bound one has no such window. The registry compares
/// instants and arms no timer, so the clock is advanced by hand and nothing
/// can jump it.
#[tokio::test]
async fn i705_5_unbound_subscription_expires_after_the_bind_window_and_a_bound_one_does_not() {
    let mut core = Core::start().await;
    let scope = scope_with_page(&core);
    let a = core.connect("native-a").await;
    let mut live = a.live(&core, &scope).await;
    assert_eq!(live.data.frame().await.0.sequence, 1);
    pause();
    let mut unbound = Vec::new();
    for _ in 1..MAX_FRAME_SUBSCRIPTIONS {
        unbound.push(a.subscribe(&scope).await);
    }
    assert_eq!(
        a.subscribe_raw(&scope).await.unwrap(),
        ObservationResult::State(ObservationState::Capacity)
    );

    tokio::time::advance(Duration::from_millis(u64::from(data::BIND_WINDOW_MS) + 1)).await;

    for sub in &unbound {
        assert_eq!(
            a.subscription(&sub.subscription).await.unwrap_err(),
            ErrorCode::PermissionDenied
        );
    }
    assert!(matches!(
        a.subscription(&live.sub.subscription).await.unwrap(),
        ObservationReply::Subscription {
            token: None,
            state: ObservationState::Live,
            ..
        }
    ));
    // seven places are free again; the bound subscription keeps its own
    for _ in 1..MAX_FRAME_SUBSCRIPTIONS {
        a.subscribe(&scope).await;
    }
    assert_eq!(
        a.subscribe_raw(&scope).await.unwrap(),
        ObservationResult::State(ObservationState::Capacity)
    );
    core.shutdown_and_join().await;
}

/// A reader that takes the frame and never acknowledges it is cut off after
/// the no-progress window. It is not told why (its socket may be full), so
/// the stream ends without a closing record, and the place comes back.
#[tokio::test]
async fn i705_5_withheld_ack_is_cut_off_at_the_no_progress_window_without_a_closing_record() {
    let mut core = Core::start().await;
    let scope = scope_with_page(&core);
    let a = core.connect("native-a").await;
    let mut live = a.live(&core, &scope).await;
    pause();
    let before = tokio::time::Instant::now();
    match live.data.item().await {
        Item::Frame(start, rgba) => {
            assert_eq!((start.sequence, rgba), (1, vec![1, 2, 3, 255]));
        }
        other => panic!("expected the frame: {other:?}"),
    }
    // no ACK is sent; the core gives up and closes without a word
    assert!(matches!(live.data.item().await, Item::Eof));
    assert!(before.elapsed() >= Duration::from_millis(u64::from(NO_PROGRESS_MS)));
    core.wait_ended(live.data.conn).await;
    assert_eq!(
        a.subscription(&live.sub.subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    for _ in 0..MAX_FRAME_SUBSCRIPTIONS {
        a.subscribe(&scope).await;
    }
}

/// A reader that is slow to acknowledge skips what came in between and
/// continues with the latest: frames are never queued behind it.
#[tokio::test]
async fn i705_5_slow_reader_skips_to_the_latest_frame_and_never_queues() {
    let core = Core::start().await;
    let scope = scope_with_page(&core);
    let a = core.connect("native-a").await;
    let mut live = a.live(&core, &scope).await;

    let Item::Frame(first, _) = live.data.item().await else {
        panic!("expected the first frame")
    };
    assert_eq!(first.sequence, 1);
    // three newer pages arrive while the receipt is outstanding. After each
    // one the control plane answers with its revision: time passes, in which
    // any task of the pump that queued what it saw could run, and the page is
    // known to be published.
    for sequence in 2..=4u64 {
        assert!(core.port.publish(page(
            &scope,
            'a',
            sequence,
            sequence,
            &format!("rev-{sequence}"),
            [sequence as u8, 0, 0, 255]
        )));
        assert_eq!(
            title(&a.get(&scope).await.unwrap()),
            format!("rev-{sequence}")
        );
    }
    ack(&mut live.data.stream, first.sequence).await;
    let (second, rgba) = live.data.frame().await;
    assert_eq!(second.sequence, 4, "frames 2 and 3 were skipped");
    assert_eq!(rgba, [4, 0, 0, 255]);
    assert_eq!(title(&a.get(&scope).await.unwrap()), "rev-4");
}

/// The eight places are one budget. A subscription that is only registered,
/// a bound one and one that ended but whose socket is still closing all count;
/// the ninth is refused until the closing one is gone. Refusing does not
/// disturb the others.
#[tokio::test]
async fn i705_5_eight_shared_places_count_registered_bound_and_closing_subscriptions() {
    assert_frames_exceed_socket_buffers();
    let mut core = Core::start().await;
    let scope = scope_with_page(&core);
    let a = core.connect("native-a").await;
    let mut live = a.live(&core, &scope).await;
    assert_eq!(live.data.frame().await.0.sequence, 1);
    let mut registered = Vec::new();
    for _ in 1..MAX_FRAME_SUBSCRIPTIONS {
        registered.push(a.subscribe(&scope).await);
    }
    assert_eq!(
        a.subscribe_raw(&scope).await.unwrap(),
        ObservationResult::State(ObservationState::Capacity)
    );
    // a refusal leaves the bound stream as it was
    assert!(
        core.port
            .publish(page(&scope, 'a', 2, 2, "second", [5, 5, 5, 255]))
    );
    assert_eq!(live.data.frame().await.0.sequence, 2);

    // the reader stops half way through the largest frame
    assert!(core.port.publish(big_page(&scope, 3, 3)));
    assert_eq!(live.data.read_start_only().await.sequence, 3);
    assert_eq!(
        a.unsubscribe(&live.sub.subscription).await.unwrap(),
        ObservationResult::Unsubscribed {}
    );
    // the subscription is gone but its socket is not: the place is still taken
    assert_eq!(
        a.subscribe_raw(&scope).await.unwrap(),
        ObservationResult::State(ObservationState::Capacity)
    );
    // the reader makes progress: the frame in flight is finished, then the
    // stream says why it ends
    let rest = live.data.rest().await;
    assert!(
        ends_with_error(&rest, data::ErrorCode::Closed),
        "the finished record is followed by Closed"
    );
    core.wait_ended(live.data.conn).await;
    let again = a.subscribe(&scope).await;
    assert_ne!(again.subscription, live.sub.subscription);
    assert_eq!(
        a.subscribe_raw(&scope).await.unwrap(),
        ObservationResult::State(ObservationState::Capacity)
    );
}

/// The core's shutdown ends every connection it serves: bound streams stop
/// at a record boundary without a word, unbound channels and the control
/// connections return, and afterwards every place is free.
#[tokio::test]
async fn i705_5_shutdown_joins_every_connection_and_releases_every_place() {
    let mut core = Core::start().await;
    let scope = scope_with_page(&core);
    let a = core.connect("native-a").await;
    let b = core.connect("native-b").await;
    let mut live_a = a.live(&core, &scope).await;
    let mut live_b = b.live(&core, &scope).await;
    assert_eq!(live_a.data.frame().await.0.sequence, 1);
    assert_eq!(live_b.data.frame().await.0.sequence, 1);
    let _registered = a.subscribe(&scope).await;
    let mut waiting = core.data().await;

    core.shutdown_and_join().await;

    for data in [&mut live_a.data, &mut live_b.data, &mut waiting] {
        assert!(
            matches!(data.item().await, Item::Eof),
            "a core that shuts down says nothing more"
        );
    }
    // the socket server is stopped, so a later client is served in process
    let _held = core.connections.open(id("native-c"));
    for _ in 0..MAX_FRAME_SUBSCRIPTIONS {
        assert!(matches!(
            core.subscribe_direct("native-c", &scope),
            ObservationResult::Subscribed { .. }
        ));
    }
    assert_eq!(
        core.subscribe_direct("native-c", &scope),
        ObservationResult::State(ObservationState::Capacity)
    );
}
