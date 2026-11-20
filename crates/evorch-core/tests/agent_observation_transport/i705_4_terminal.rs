//! #705 item 4: terminal states and generations. A stream, a token or a scope
//! of something that ended is never revived by what replaces it.
//!
//! What this fake source can and cannot say: a page that ends is `revoke`d
//! (the agent's browser, page or run incarnation is gone) or published with a
//! terminal state. The authority that maps run phases (Done, Error, Stopped,
//! Stop request) and stored membership onto those states is private to the
//! service and replaced by a permissive one here; the host's service tests
//! drive the real one.
use crate::harness::*;
use workbench_protocol::{
    errors::ErrorCode,
    ids::{ProjectId, ThreadId},
    namespaces::browser::{BrowserQuery, observation::*},
};

/// A terminal publication ends the live stream with a reason and no further
/// frame, refuses new subscriptions with that state, and is not undone by a
/// later live publication of the same scope.
#[tokio::test]
async fn i705_4_terminal_publication_ends_streams_and_is_not_revived_by_a_later_page() {
    let mut core = Core::start().await;
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "live", [1, 2, 3, 255]))
    );
    let a = core.connect("native-a").await;
    let mut live = a.live(&core, &scope).await;
    assert_eq!(live.data.frame().await.0.sequence, 1);

    // the same frame sequence: nothing new for the stream, only the state
    let stopped = with_state(
        page(&scope, 'a', 2, 1, "stopped", [1, 2, 3, 255]),
        ObservationState::Stopped,
    );
    assert!(core.port.publish(stopped));
    // the next control request settles the registry (production also does it
    // from its lifecycle task, which the fake service does not start)
    assert_eq!(
        a.get(&scope).await.unwrap(),
        ObservationReply::State(ObservationState::Stopped)
    );
    live.data.expect_closed().await;
    core.wait_ended(live.data.conn).await;
    assert_eq!(
        a.subscribe_raw(&scope).await.unwrap(),
        ObservationResult::State(ObservationState::Stopped)
    );
    assert_eq!(
        a.subscription(&live.sub.subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );

    // a later live page of the same scope does not give the old stream, token
    // or subscription back
    assert!(
        core.port
            .publish(page(&scope, 'a', 3, 2, "again", [9, 9, 9, 255]))
    );
    assert_eq!(
        a.subscription(&live.sub.subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    let spare = core.data().await;
    assert_eq!(
        a.bind(&spare.channel, &live.sub.subscription, &live.sub.token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
}

/// The agent's page, browser or run incarnation is replaced: the old scope is
/// revoked everywhere, its stream ends without ever carrying the new pixels,
/// and the new scope starts from its own first frame.
#[tokio::test]
async fn i705_4_replaced_incarnation_browser_or_page_generation_never_leaks_old_payload() {
    for (name, change) in [
        ("incarnation", Change::Incarnation),
        ("browser generation", Change::Browser),
        ("page generation", Change::Page),
    ] {
        let mut core = Core::start().await;
        let old_scope = core.scope("run-1", "inc-1", 1);
        let mut new_scope = old_scope.clone();
        match change {
            Change::Incarnation => {
                new_scope.incarnation = workbench_protocol::ids::RunId::new("inc-2").unwrap();
            }
            Change::Browser => new_scope.browser_generation += 1,
            Change::Page => new_scope.page_generation += 1,
        }
        assert!(
            core.port
                .publish(page(&old_scope, 'a', 1, 1, "old", [1, 1, 1, 255]))
        );
        let a = core.connect("native-a").await;
        let mut old = a.live(&core, &old_scope).await;
        let (start, rgba) = old.data.frame().await;
        assert_eq!(
            (start.scope, rgba),
            (old_scope.clone(), vec![1, 1, 1, 255]),
            "{name}"
        );

        // the replacement is published before the old page is gone: a request
        // in between may see both scopes, never a mix
        assert!(
            core.port
                .publish(page(&new_scope, 'b', 1, 1, "new", [2, 2, 2, 255]))
        );
        core.port.revoke(&old_scope);

        old.data.expect_closed().await;
        core.wait_ended(old.data.conn).await;
        assert_eq!(
            a.get(&old_scope).await.unwrap(),
            ObservationReply::State(ObservationState::Revoked),
            "{name}"
        );
        assert_eq!(
            a.subscribe_raw(&old_scope).await.unwrap(),
            ObservationResult::State(ObservationState::Revoked),
            "{name}"
        );
        assert_eq!(
            a.subscription(&old.sub.subscription).await.unwrap_err(),
            ErrorCode::PermissionDenied,
            "{name}"
        );
        assert_eq!(
            a.list().await.unwrap(),
            ObservationReply::Runs {
                scopes: vec![new_scope.clone()]
            },
            "{name}"
        );

        let mut new = a.live(&core, &new_scope).await;
        let (start, rgba) = new.data.frame().await;
        assert_eq!(start.scope, new_scope, "{name}");
        assert_eq!(rgba, [2, 2, 2, 255], "{name}: only the new pixels");
        assert_eq!(title(&a.get(&new_scope).await.unwrap()), "new", "{name}");
    }
}

#[derive(Clone, Copy)]
enum Change {
    Incarnation,
    Browser,
    Page,
}

/// A scope names its project and thread. Another thread or project is not
/// served what the page of this one shows, however it is asked.
#[tokio::test]
async fn i705_4_scope_of_another_thread_or_project_is_not_served() {
    let core = Core::start().await;
    let scope = core.scope("run-1", "inc-1", 1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "mine", [1, 2, 3, 255]))
    );
    let a = core.connect("native-a").await;

    let mut other_thread = scope.clone();
    other_thread.thread = ThreadId::new("t2").unwrap();
    let mut other_project = scope.clone();
    other_project.project = ProjectId::new("p2").unwrap();
    for other in [&other_thread, &other_project] {
        assert_eq!(
            a.get(other).await.unwrap(),
            ObservationReply::State(ObservationState::Revoked)
        );
        assert_eq!(
            a.subscribe_raw(other).await.unwrap(),
            ObservationResult::State(ObservationState::Revoked)
        );
    }
    assert_eq!(
        a.query(BrowserQuery::ListAgentRuns {
            project: scope.project.clone(),
            thread: ThreadId::new("t2").unwrap(),
        })
        .await
        .unwrap(),
        ObservationReply::Runs { scopes: vec![] }
    );
}

// ---- real run phases and stored membership, through the production service ---
//
// The service below is the production one over the fake port: its authority
// reads the real runtime (phase, Stop request, execution) and the stored
// thread and sidebar. Each test changes the real thing and then looks at the
// sockets.

struct Observed {
    a: Client,
    live: Live,
    scope: ObservationScope,
}

async fn observe(core: &Core) -> Observed {
    let scope = core.real_scope(1);
    assert!(
        core.port
            .publish(page(&scope, 'a', 1, 1, "live", [1, 2, 3, 255]))
    );
    let a = core.connect("native-a").await;
    let mut live = a.live(core, &scope).await;
    assert_eq!(live.data.frame().await.0.sequence, 1);
    Observed { a, live, scope }
}

/// The old stream, token, subscription and scope are dead on both sockets, and
/// the control plane says `state`.
async fn assert_ended(core: &mut Core, observed: &mut Observed, state: ObservationState) {
    // the service settles its registry on any request (and on its own events)
    let _ = observed.a.list().await;
    observed.live.data.expect_closed().await;
    core.wait_ended(observed.live.data.conn).await;
    let Observed { a, live, scope } = observed;
    assert_eq!(
        a.subscription(&live.sub.subscription).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );
    let spare = core.data().await;
    assert_eq!(
        a.bind(&spare.channel, &live.sub.subscription, &live.sub.token)
            .await
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    assert_eq!(a.get(scope).await.unwrap(), ObservationReply::State(state));
    assert_eq!(
        a.subscribe_raw(scope).await.unwrap(),
        ObservationResult::State(state)
    );
}

#[tokio::test]
async fn i705_4_real_done_run_ends_the_stream_and_refuses_the_scope() {
    let mut core = Core::start_real().await;
    let mut observed = observe(&core).await;
    let real = core.real.as_ref().unwrap();
    real.model.answer();
    let run = real.run;
    assert_eq!(
        core.runtime().wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    assert_ended(&mut core, &mut observed, ObservationState::Stopped).await;
}

#[tokio::test]
async fn i705_4_real_failed_run_ends_the_stream_and_refuses_the_scope() {
    let mut core = Core::start_real().await;
    let mut observed = observe(&core).await;
    let real = core.real.as_ref().unwrap();
    real.model.fail();
    let run = real.run;
    assert_eq!(
        core.runtime().wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Error
    );
    assert_ended(&mut core, &mut observed, ObservationState::Stopped).await;
}

#[tokio::test]
async fn i705_4_real_stop_request_ends_the_stream_while_the_run_is_still_in_flight() {
    let mut core = Core::start_real().await;
    let mut observed = observe(&core).await;
    let run = core.real.as_ref().unwrap().run;
    core.runtime()
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    assert!(core.runtime().graceful_stop_requested(run));
    assert_ended(&mut core, &mut observed, ObservationState::Stopped).await;
}

#[tokio::test]
async fn i705_4_real_cancelled_run_ends_the_stream_and_refuses_the_scope() {
    let mut core = Core::start_real().await;
    let mut observed = observe(&core).await;
    let run = core.real.as_ref().unwrap().run;
    core.runtime().cancel(run).unwrap();
    core.runtime().wait(run).await.unwrap();
    assert_ended(&mut core, &mut observed, ObservationState::Revoked).await;
}

#[tokio::test]
async fn i705_4_real_removal_of_the_project_ends_the_stream_and_refuses_the_scope() {
    let mut core = Core::start_real().await;
    let mut observed = observe(&core).await;
    write_sidebar(&core.real.as_ref().unwrap().store, &[]);
    assert_ended(&mut core, &mut observed, ObservationState::Revoked).await;
}

#[tokio::test]
async fn i705_4_real_reaffiliation_of_the_thread_ends_the_stream_and_refuses_the_scope() {
    let mut core = Core::start_real().await;
    let mut observed = observe(&core).await;
    let real = core.real.as_ref().unwrap();
    let elsewhere = real.project_root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    real.upsert_thread("p1", &elsewhere);
    assert_ended(&mut core, &mut observed, ObservationState::Revoked).await;
}
