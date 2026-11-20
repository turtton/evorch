//! #705 item 2: what the transport refuses, on the control socket AND on the
//! data socket. The boundary is the peer UID (checked by `unix::connection`,
//! see the file header for what a test can reach), the client id and the
//! connection epoch the core derives from the connection, and the private
//! single-use token.
use crate::harness::*;
use workbench_protocol::{
    commands::Command,
    envelope::{Provenance, ReplyResult, RequestBody, RequestEnvelope},
    errors::ErrorCode,
    ids::{RequestId, RunId, SessionId},
    namespaces::browser::{BrowserCommand, BrowserQuery, observation::*},
    queries::Query,
};

fn live_scope(core: &Core) -> ObservationScope {
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "first", [1, 2, 3, 255]))
    );
    scope
}

/// Another client that learned a public subscription id, even together with
/// the owner's token, can neither read, unsubscribe nor bind it. A refused
/// theft burns nothing: the owner still binds, and the thief's channel stays
/// silent until its window ends.
#[tokio::test]
async fn i705_2_other_client_with_public_id_and_stolen_token_cannot_read_unsubscribe_or_bind() {
    let core = Core::start().await;
    let scope = live_scope(&core);
    let a = core.connect("native-a").await;
    let b = core.connect("native-b").await;
    let sub = a.subscribe(&scope).await;

    // control socket
    assert_eq!(
        b.subscription(&sub.subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        b.unsubscribe(&sub.subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    let mut thief = core.data().await;
    assert_eq!(
        b.bind(&thief.channel, &sub.subscription, &sub.token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // a wrong token of the owner's own id is no better
    let other = b.subscribe(&scope).await;
    assert_ne!(other.token, sub.token);
    assert_eq!(
        a.bind(&thief.channel, &sub.subscription, &other.token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );

    // the owner is unharmed and gets exactly its own stream
    let mut owner = core.data().await;
    assert!(
        a.bind(&owner.channel, &sub.subscription, &sub.token)
            .await
            .is_ok()
    );
    let (start, rgba) = owner.frame().await;
    assert_eq!((start.sequence, rgba), (1, vec![1, 2, 3, 255]));

    // data socket: the refused channel carried nothing and ends by its window
    pause();
    thief.expect_expired_after_the_window().await;
}

/// A subscription belongs to the connection epoch it was made in. After the
/// client's last connection ended it is gone for every later connection of the
/// same client id; a scope of another core epoch is not served at all.
#[tokio::test]
async fn i705_2_old_epoch_and_forged_core_epoch_are_refused() {
    let mut core = Core::start().await;
    let scope = live_scope(&core);
    let a = core.connect("native-a").await;
    let mut old = a.live(&core, &scope).await;
    assert_eq!(old.data.frame().await.0.sequence, 1);
    let (conn, token, subscription) = (a.conn, old.sub.token.clone(), old.sub.subscription.clone());
    drop(a);
    core.wait_ended(conn).await;
    // data socket: the old epoch's stream is closed, not left readable
    old.data.expect_closed().await;

    // the same client id again: a new epoch that owns nothing of the old one
    let a = core.connect("native-a").await;
    assert_eq!(
        a.subscription(&subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        a.unsubscribe(&subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    let fresh = core.data().await;
    assert_eq!(
        a.bind(&fresh.channel, &subscription, &token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    let next = a.subscribe(&scope).await;
    assert_ne!(next.subscription, subscription);
    assert_ne!(next.token, token);

    // a scope of another core instance is revoked on the control plane and is
    // refused locally by the data client before any command is sent
    let mut stale = scope.clone();
    stale.core_epoch += 1;
    assert_eq!(
        a.subscribe_raw(&stale).await.unwrap(),
        ObservationResult::State(ObservationState::Revoked)
    );
    assert_eq!(
        a.get(&stale).await.unwrap(),
        ObservationReply::State(ObservationState::Revoked)
    );
    let channel = workbench_client::renderer::DataChannel::connect(&core.socket)
        .await
        .unwrap();
    assert!(
        channel
            .bind_agent_frames(
                &a.control,
                stale,
                next.subscription.clone(),
                next.token.clone()
            )
            .await
            .is_err()
    );
    // nothing was consumed by the refused attempt
    let mut data = core.data().await;
    assert!(
        a.bind(&data.channel, &next.subscription, &next.token)
            .await
            .is_ok()
    );
    assert_eq!(data.frame().await.0.sequence, 1);
}

/// A token binds once. It is not reusable after the bind, after the
/// subscription ended, or for another subscription; every refused channel
/// stays silent.
#[tokio::test]
async fn i705_2_token_reuse_is_refused_after_bind_unsubscribe_and_for_another_subscription() {
    let mut core = Core::start().await;
    let scope = live_scope(&core);
    let a = core.connect("native-a").await;
    let mut first = a.live(&core, &scope).await;
    assert_eq!(first.data.frame().await.0.sequence, 1);
    let second = a.subscribe(&scope).await;

    let reuse = core.data().await;
    assert_eq!(
        a.bind(&reuse.channel, &first.sub.subscription, &first.sub.token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // a token of one subscription does not open another
    assert_eq!(
        a.bind(&reuse.channel, &second.subscription, &first.sub.token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // after unsubscribe nothing of it works
    a.unsubscribe(&first.sub.subscription).await.unwrap();
    first.data.expect_closed().await;
    core.wait_ended(first.data.conn).await;
    assert_eq!(
        a.bind(&reuse.channel, &first.sub.subscription, &first.sub.token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // the second subscription is untouched by all of it
    let mut live = core.data().await;
    assert!(
        a.bind(&live.channel, &second.subscription, &second.token)
            .await
            .is_ok()
    );
    assert_eq!(live.frame().await.0.sequence, 1);

    let mut reuse = reuse;
    pause();
    reuse.expect_expired_after_the_window().await;
}

/// Surface and actor come from the transport. Web, CLI, an agent and a client
/// the core never saw connect cannot reach agent observation by any command or
/// query, and nothing they sent is registered: the eight places are all free.
///
/// A client cannot name its surface or actor on the wire (the envelope rejects
/// the field), so these provenances are given to the core's own dispatch, the
/// way the Web adapter and the agent runtime reach it.
#[tokio::test]
async fn i705_2_web_agent_cli_and_unknown_clients_cannot_observe() {
    let core = Core::start().await;
    let scope = live_scope(&core);
    let a = core.connect("native-a").await;
    let sub = a.subscribe(&scope).await;
    let data = core.data().await;

    let provenances = [
        Provenance::human(workbench_protocol::envelope::Surface::Web, id("native-a")),
        Provenance::human(workbench_protocol::envelope::Surface::Cli, id("native-a")),
        Provenance::human(workbench_protocol::envelope::Surface::None, id("native-a")),
        Provenance::human(
            workbench_protocol::envelope::Surface::Desktop,
            id("never-connected"),
        ),
        Provenance::agent(RunId::new("run-1").unwrap()),
    ];
    let commands = [
        BrowserCommand::SubscribeAgentFrames {
            scope: scope.clone(),
        },
        BrowserCommand::UnsubscribeAgentFrames {
            subscription: sub.subscription.clone(),
        },
        BrowserCommand::BindAgentData {
            channel_id: data.channel.clone(),
            subscription: sub.subscription.clone(),
            token: sub.token.clone(),
        },
    ];
    let queries = [
        BrowserQuery::ListAgentRuns {
            project: workbench_protocol::ids::ProjectId::new(PROJECT).unwrap(),
            thread: workbench_protocol::ids::ThreadId::new(THREAD).unwrap(),
        },
        BrowserQuery::GetAgentObservation {
            scope: scope.clone(),
        },
        BrowserQuery::GetAgentSubscription {
            subscription: sub.subscription.clone(),
        },
    ];
    let mut n = 0;
    let mut request = |body: RequestBody| {
        n += 1;
        RequestEnvelope {
            request_id: RequestId::new(format!("forged-{n}")).unwrap(),
            session_id: SessionId::new("forged-session").unwrap(),
            body,
        }
    };
    for provenance in &provenances {
        for command in &commands {
            let reply = core.backend.request(
                request(RequestBody::Command(Command::Browser(command.clone()))),
                provenance.clone(),
            );
            assert_denied(&reply.result, provenance, command);
        }
        for query in &queries {
            let reply = core.backend.request(
                request(RequestBody::Query(Query::Browser(query.clone()))),
                provenance.clone(),
            );
            assert_denied(&reply.result, provenance, query);
        }
    }

    // nothing was registered or consumed: the owner's token still binds
    let mut owner = data;
    assert!(
        a.bind(&owner.channel, &sub.subscription, &sub.token)
            .await
            .is_ok()
    );
    assert_eq!(owner.frame().await.0.sequence, 1);
    for _ in 0..7 {
        a.subscribe(&scope).await;
    }
}

fn assert_denied(
    result: &ReplyResult,
    provenance: &Provenance,
    what: &(impl std::fmt::Debug + ?Sized),
) {
    match result {
        ReplyResult::Error(error) => assert!(
            matches!(
                error.code,
                ErrorCode::PermissionDenied | ErrorCode::InvalidRequest
            ),
            "{provenance:?} {what:?}: {:?}",
            error.code
        ),
        ReplyResult::Success(_) => panic!("{provenance:?} was served {what:?}"),
    }
}

/// A data connection speaks to nobody before it is bound: bytes sent first end
/// it silently, and it never receives a frame.
#[tokio::test]
async fn i705_2_data_connection_cannot_speak_before_it_is_bound() {
    let core = Core::start().await;
    let scope = live_scope(&core);
    let a = core.connect("native-a").await;
    let _sub = a.subscribe(&scope).await;
    let mut data = core.data().await;
    ack(&mut data.stream, 1).await;
    assert!(
        matches!(data.item().await, Item::Eof),
        "the core ends the connection without a record"
    );
}
