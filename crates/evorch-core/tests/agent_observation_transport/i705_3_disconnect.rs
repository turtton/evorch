//! #705 item 3: kill, disconnect, reconnect and a connection-closed callback
//! that reaches the service late. Only the departing client's subscriptions
//! end, and none of it closes or restamps the agent's page.
use crate::harness::*;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use workbench_host::{
    areas::browser::observation::AgentObservationPort, connections::ConnectionClosedHook,
};
use workbench_protocol::errors::ErrorCode;

/// A client's last connection ends: its stream is closed with a reason, a
/// bystander's keeps flowing, and the agent's page stays observable.
#[tokio::test]
async fn i705_3_last_disconnect_ends_only_that_client_and_leaves_the_agent_page_open() {
    let mut core = Core::start().await;
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "first", [1, 2, 3, 255]))
    );
    let a = core.connect("native-a").await;
    let b = core.connect("native-b").await;
    let mut live_a = a.live(&core, &scope).await;
    let mut live_b = b.live(&core, &scope).await;
    assert_eq!(live_a.data.frame().await.0.sequence, 1);
    assert_eq!(live_b.data.frame().await.0.sequence, 1);

    let (conn, subscription) = (a.conn, live_a.sub.subscription.clone());
    drop(a);
    core.wait_ended(conn).await;
    live_a.data.expect_closed().await;
    core.wait_ended(live_a.data.conn).await;

    // the bystander is untouched and still receives what the agent shows next
    assert!(
        core.port
            .publish(page(&scope, 'a', 2, 2, "second", [4, 5, 6, 255]))
    );
    let (start, rgba) = live_b.data.frame().await;
    assert_eq!((start.sequence, rgba), (2, vec![4, 5, 6, 255]));
    assert!(b.subscription(&live_b.sub.subscription).await.is_ok());
    assert_eq!(
        b.subscription(&subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // observing is not owning: the agent's page is open and readable
    assert!(core.port.current(&scope));
    assert_eq!(title(&b.get(&scope).await.unwrap()), "second");
}

/// A client may hold several connections: the hooks run when the last one
/// ends, so closing the first one leaves its subscription and stream alive.
#[tokio::test]
async fn i705_3_client_with_two_connections_ends_only_with_its_last_connection() {
    let mut core = Core::start().await;
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "first", [1, 2, 3, 255]))
    );
    let first = core.connect("native-a").await;
    let second = core.connect("native-a").await;
    let mut live = first.live(&core, &scope).await;
    assert_eq!(live.data.frame().await.0.sequence, 1);

    let conn = first.conn;
    drop(first);
    core.wait_ended(conn).await;
    // same client, same epoch: the subscription is still its own
    assert!(second.subscription(&live.sub.subscription).await.is_ok());
    assert!(
        core.port
            .publish(page(&scope, 'a', 2, 2, "second", [4, 5, 6, 255]))
    );
    assert_eq!(live.data.frame().await.0.sequence, 2);

    drop(second);
    live.data.expect_closed().await;
    core.wait_ended(live.data.conn).await;
}

/// A control connection killed where it stands (its task aborted, nothing
/// said) ends the client's observation exactly like a clean disconnect.
#[tokio::test]
async fn i705_3_killed_control_connection_revokes_like_a_disconnect_and_spares_others() {
    let core = Core::start().await;
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "first", [1, 2, 3, 255]))
    );
    let a = core.connect("native-a").await;
    let b = core.connect("native-b").await;
    let mut live_a = a.live(&core, &scope).await;
    let mut live_b = b.live(&core, &scope).await;
    assert_eq!(live_a.data.frame().await.0.sequence, 1);
    assert_eq!(live_b.data.frame().await.0.sequence, 1);

    core.kill_connection(a.conn);
    live_a.data.expect_closed().await;
    assert!(
        core.port
            .publish(page(&scope, 'a', 2, 2, "second", [4, 5, 6, 255]))
    );
    assert_eq!(live_b.data.frame().await.0.sequence, 2);
    assert!(core.port.current(&scope));
}

/// The connection-closed callback of an OLD epoch reaches the service after
/// the same client came back. It must end nothing of the new epoch.
///
/// The callback is held back by a hook that runs before the service's own and
/// waits for the test; the client reconnects, subscribes and binds in the
/// meantime. A hook after the service's reports that the late callback ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn i705_3_late_callback_of_an_old_epoch_does_not_end_the_new_epoch() {
    let (entered_tx, mut entered) = mpsc::unbounded_channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let (finished_tx, mut finished) = mpsc::unbounded_channel::<()>();
    let first: ConnectionClosedHook = Arc::new(move |client, _| {
        if client.as_str() == "late-client" {
            entered_tx.send(()).unwrap();
            // the hook runs in the connection task's teardown: wait in a way
            // that hands the worker's queue (and the tasks of the new
            // connection in it) to another thread
            tokio::task::block_in_place(|| release_rx.lock().unwrap().recv().unwrap());
        }
    });
    let last: ConnectionClosedHook = Arc::new(move |client, _| {
        if client.as_str() == "late-client" {
            finished_tx.send(()).unwrap();
        }
    });
    let core = Core::start_with(Hooks {
        first: Some(first),
        last: Some(last),
    })
    .await;
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "first", [1, 2, 3, 255]))
    );
    let bystander = core.connect("native-b").await;
    let mut live_bystander = bystander.live(&core, &scope).await;
    assert_eq!(live_bystander.data.frame().await.0.sequence, 1);

    let old = core.connect("late-client").await;
    let mut old_live = old.live(&core, &scope).await;
    assert_eq!(old_live.data.frame().await.0.sequence, 1);
    let old_epoch = core.connections.epoch_of(&old.name).unwrap();

    // the old epoch ends; its callback is now held in the first hook
    drop(old);
    entered.recv().await.unwrap();
    assert_eq!(core.connections.epoch_of(&id("late-client")), None);

    // the client is back: a later epoch with a subscription of its own
    let new = core.connect("late-client").await;
    let mut new_live = new.live(&core, &scope).await;
    assert_eq!(new_live.data.frame().await.0.sequence, 1);
    // the core registers a connection after its handshake reply, so the epoch
    // is read only once a request of it has been served
    assert!(core.connections.epoch_of(&new.name).unwrap() > old_epoch);

    // the registry already ended the old epoch's stream when the new epoch
    // spoke, without waiting for the callback
    old_live.data.expect_closed().await;

    // the late callback runs now, between the new epoch's frames
    release_tx.send(()).unwrap();
    finished.recv().await.unwrap();
    assert!(
        core.port
            .publish(page(&scope, 'a', 2, 2, "second", [4, 5, 6, 255]))
    );
    assert_eq!(new_live.data.frame().await.0.sequence, 2);
    assert!(new.subscription(&new_live.sub.subscription).await.is_ok());
    assert_eq!(live_bystander.data.frame().await.0.sequence, 2);
    // and the old epoch stays gone
    assert_eq!(
        new.subscription(&old_live.sub.subscription)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    assert!(core.port.current(&scope));
}
