//! #705 item 1: the whole normal path over the real sockets.
use crate::harness::*;
use workbench_host::areas::browser::observation::AgentObservationPort;
use workbench_protocol::{errors::ErrorCode, namespaces::browser::observation::*};

/// native list/query → subscribe → private token → single-use bind →
/// RGBA/ACK → metadata → unsubscribe, on one core.
#[tokio::test]
async fn i705_1_list_subscribe_token_single_use_bind_rgba_ack_metadata_unsubscribe() {
    let mut core = Core::start().await;
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "first", [1, 2, 3, 255]))
    );
    let a = core.connect("native-a").await;

    // list / query
    assert_eq!(
        a.list().await.unwrap(),
        ObservationReply::Runs {
            scopes: vec![scope.clone()]
        }
    );
    let snapshot = a.get(&scope).await.unwrap();
    assert_eq!(title(&snapshot), "first");

    // subscribe: registration only, the token is private to the owner
    let sub = a.subscribe(&scope).await;
    assert_eq!(
        a.subscription(&sub.subscription).await.unwrap(),
        ObservationReply::Subscription {
            subscription: sub.subscription.clone(),
            scope: scope.clone(),
            token: Some(sub.token.clone()),
            state: ObservationState::Starting,
        }
    );

    // bind: a data connection offers a channel, the control connection binds it
    let mut data = core.data().await;
    assert_eq!(
        a.bind(&data.channel, &sub.subscription, &sub.token)
            .await
            .unwrap(),
        ObservationResult::Bound {}
    );
    // the token is single use: another channel cannot take it again
    let spare = core.data().await;
    assert_eq!(
        a.bind(&spare.channel, &sub.subscription, &sub.token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // and the bound subscription no longer offers it
    assert!(matches!(
        a.subscription(&sub.subscription).await.unwrap(),
        ObservationReply::Subscription {
            token: None,
            state: ObservationState::Live,
            ..
        }
    ));

    // RGBA and its ACK: the first frame is the page as published
    let (start, rgba) = data.frame().await;
    assert_eq!(start.scope, scope);
    assert_eq!((start.sequence, start.width, start.height), (1, 1, 1));
    assert_eq!(rgba, [1, 2, 3, 255]);

    // the next publication reaches the same stream, after the ACK was consumed
    assert!(
        core.port
            .publish(page(&scope, 'a', 2, 2, "second", [9, 8, 7, 255]))
    );
    let (start, rgba) = data.frame().await;
    assert_eq!(start.sequence, 2);
    assert_eq!(rgba, [9, 8, 7, 255]);

    // metadata: the control plane serves the newer revision of the same page
    match a.get(&scope).await.unwrap() {
        ObservationReply::Snapshot(snapshot) => {
            assert_eq!(snapshot.revision, 2);
            assert_eq!(snapshot.tabs[0].title, "second");
        }
        other => panic!("not a snapshot: {other:?}"),
    }

    // unsubscribe: the stream says why it ends and ends
    assert_eq!(
        a.unsubscribe(&sub.subscription).await.unwrap(),
        ObservationResult::Unsubscribed {}
    );
    data.expect_closed().await;
    core.wait_ended(data.conn).await;
    assert_eq!(
        a.subscription(&sub.subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // observing is not owning: the agent's page is still there, and readable
    assert!(core.port.current(&scope));
    assert!(matches!(
        a.get(&scope).await.unwrap(),
        ObservationReply::Snapshot(_)
    ));
}

/// The metadata notification of the normal path, through the production
/// service: opaque subscription id, revision and coarse state reach a control
/// subscription of the real core, through its projection, when the
/// subscription is registered, bound, updated and ended. Nothing private rides
/// along.
#[tokio::test]
async fn i705_1_metadata_notifications_reach_a_control_subscription() {
    use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
    use workbench_protocol::namespaces::browser::BrowserEvent;

    async fn event_of(
        events: &mut workbench_client::Subscription,
        subscription: &ObservationId,
    ) -> ObservationEvent {
        loop {
            let envelope = events.next().await.expect("the subscription stays open");
            if let UiEvent::Browser(BrowserEvent::AgentObservationChanged(event)) = envelope.event
                && &event.subscription == subscription
            {
                return event;
            }
        }
    }

    let core = Core::start_real().await;
    let scope = core.real_scope(1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "first", [1, 2, 3, 255]))
    );
    let a = core.connect("native-a").await;
    let mut events = a
        .control
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .unwrap();

    // registered: Starting at the revision of the page
    let sub = a.subscribe(&scope).await;
    let starting = event_of(&mut events, &sub.subscription).await;
    assert_eq!(
        (starting.revision, starting.state),
        (1, ObservationState::Starting)
    );

    // bound: Live
    let mut data = core.data().await;
    a.bind(&data.channel, &sub.subscription, &sub.token)
        .await
        .unwrap();
    let live = event_of(&mut events, &sub.subscription).await;
    assert_eq!((live.revision, live.state), (1, ObservationState::Live));
    assert_eq!(data.frame().await.0.sequence, 1);

    // a newer revision of the metadata: announced with its revision
    assert!(
        core.port
            .publish(page(&scope, 'a', 2, 2, "second", [4, 5, 6, 255]))
    );
    let changed = event_of(&mut events, &sub.subscription).await;
    assert_eq!(
        (changed.revision, changed.state),
        (2, ObservationState::Live)
    );
    let wire = serde_json::to_string(&changed).unwrap();
    for private in [sub.token.as_str(), "second", "local-only"] {
        assert!(!wire.contains(private), "{private} is in {wire}");
    }

    assert_eq!(data.frame().await.0.sequence, 2);

    // ended by its owner: Stopped
    a.unsubscribe(&sub.subscription).await.unwrap();
    let stopped = event_of(&mut events, &sub.subscription).await;
    assert_eq!(stopped.state, ObservationState::Stopped);
    data.expect_closed().await;
}
