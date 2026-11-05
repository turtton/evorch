//! A derived thread takes its parent's link only while the pull request is confirmed open.

use event_bus::{PullRequestLinkOrigin, ToolEvent};
use storage::pull_requests::{
    InheritanceReason, InheritanceResolution, InheritanceState, LinkOrigin,
};

use super::*;
use crate::pull_requests::FetchError;

async fn next_for(process: &mut Process, thread: &str) -> PullRequestEvent {
    loop {
        let event = process.next().await;
        if event.thread_id() == thread {
            return event;
        }
    }
}

async fn linked_parent(process: &mut Process, facts_for_parent: FetchedPullRequest) {
    process.source.push_ok(&key(1), facts_for_parent);
    process.link("t1", 1).await;
    next_for(process, "t1").await;
    next_for(process, "t1").await;
}

fn record(process: &Process, child: &str) -> storage::pull_requests::InheritanceRecord {
    process
        .storage_handle
        .pull_request_inheritance(child)
        .unwrap()
        .unwrap()
}

// Given: a child whose lease moves to another process while a fetch it waits on is under way,
// a fetch that stays valid for the parent / When: the answer arrives / Then: the shared answer
// does not stand in for the child's own authority: nothing is inherited
#[tokio::test(start_paused = true)]
async fn a_child_that_lost_its_lease_during_the_fetch_does_not_take_the_link() {
    let world = World::new();
    let mut first = world.process(enabled());
    let second = world.process(enabled());
    linked_parent(&mut first, open_running(1)).await;
    let gate = first.source.push_gate(&key(1));
    let parent_permit = first.permit("t1");
    let child_permit = first.permit("c1");
    first
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    gate.arrived().await;
    first.service.refresh("t1", &parent_permit).await.unwrap();
    settle().await;

    first.host.handoff(&child_permit, &second.host).unwrap();
    gate.release_ok(open_running(1));
    settle().await;

    assert_eq!(record(&first, "c1").state, InheritanceState::Pending);
    assert!(first.state("c1").active_link.is_none());
    drop(second);
}

// Given: a pull request that cannot be fetched and a pending inheritance / When: time passes /
// Then: the attempts follow the pull request's backoff, shared with the parent's tracking
#[tokio::test(start_paused = true)]
async fn a_pending_inheritance_waits_for_the_backoff_of_its_pull_request() {
    let world = World::new();
    let mut process = world.process(enabled());
    linked_parent(&mut process, open_running(1)).await;
    process
        .source
        .push_err(&key(1), FetchError::Network("down".into()));
    process.source.repeat_last();
    process.permit("c1");

    process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        process.source.calls().len(),
        2,
        "the first attempt is made at once"
    );

    pass(59).await;
    assert_eq!(
        process.source.calls().len(),
        2,
        "the retry waits the 60 s backoff"
    );
    pass(1).await;
    assert_eq!(
        process.source.calls().len(),
        3,
        "one fetch serves the parent and the child"
    );
    pass(119).await;
    assert_eq!(process.source.calls().len(), 3, "then 120 s");
    pass(1).await;
    assert_eq!(process.source.calls().len(), 4);
    assert_eq!(record(&process, "c1").state, InheritanceState::Pending);
}

// Given: a parent with an open pull request and an owned child / When: the child is derived /
// Then: it takes the link (origin inherited, pinned to the parent's link) and is tracked
#[tokio::test(start_paused = true)]
async fn an_open_pull_request_is_inherited() {
    let world = World::new();
    let mut process = world.process(enabled());
    linked_parent(&mut process, open_running(1)).await;
    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    process.permit("c1");

    let scheduled = process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();

    assert!(matches!(scheduled, InheritanceScheduling::Scheduled(_)));
    let linked = next_for(&mut process, "c1").await;
    assert!(matches!(
        linked,
        PullRequestEvent::Linked {
            origin: PullRequestLinkOrigin::Inherited,
            ..
        }
    ));
    let link = process.state("c1").active_link.unwrap();
    assert_eq!(link.origin, LinkOrigin::Inherited);
    assert_eq!(
        link.parent_link_id,
        Some(process.state("t1").active_link.unwrap().link_id)
    );
    assert_eq!(record(&process, "c1").state, InheritanceState::Applied);
}

// Given: a parent whose pull request is merged / When: the child is derived / Then: nothing is
// inherited, and the decision is stored as skipped
#[tokio::test(start_paused = true)]
async fn a_merged_pull_request_is_not_inherited() {
    let world = World::new();
    let mut process = world.process(enabled());
    let merged = fetched(1, facts(PullRequestState::Merged, passing()));
    linked_parent(&mut process, merged.clone()).await;
    process.source.push_ok(&key(1), merged);
    process.permit("c1");

    process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    settle().await;

    let row = record(&process, "c1");
    assert_eq!(row.state, InheritanceState::Skipped);
    assert_eq!(row.decision_reason, Some(InheritanceReason::Merged));
    assert!(process.state("c1").active_link.is_none());
}

// Given: a fetch that fails while the child waits / When: time passes / Then: the
// inheritance stays pending (the failure is counted), and the next attempt applies it
#[tokio::test(start_paused = true)]
async fn an_unconfirmed_pull_request_stays_pending_until_it_can_be_confirmed() {
    let world = World::new();
    let mut process = world.process(enabled());
    linked_parent(&mut process, open_running(1)).await;
    process
        .source
        .push_err(&key(1), FetchError::Network("down".into()));
    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    process.permit("c1");

    process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    settle().await;

    let row = record(&process, "c1");
    assert_eq!(row.state, InheritanceState::Pending);
    assert_eq!(row.attempt_count, 1);
    assert!(process.state("c1").active_link.is_none());
    pass(30).await;
    assert!(matches!(
        next_for(&mut process, "c1").await,
        PullRequestEvent::Linked { .. }
    ));
    assert_eq!(record(&process, "c1").state, InheritanceState::Applied);
}

// Given: a pending inheritance whose child nobody owns / When: this process exits and a new one
// starts and owns the child / Then: the new process resolves it
#[tokio::test(start_paused = true)]
async fn a_pending_inheritance_is_resolved_after_a_restart() {
    let world = World::new();
    let mut first = world.process(enabled());
    linked_parent(&mut first, open_running(1)).await;
    first
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    pass(3_600).await;
    assert_eq!(record(&first, "c1").state, InheritanceState::Pending);
    first.service.shutdown();
    drop(first);

    let mut second = world.process(enabled());
    second.source.push_ok(&key(1), open_running(1));
    second.permit("c1");

    assert!(matches!(
        next_for(&mut second, "c1").await,
        PullRequestEvent::Linked {
            origin: PullRequestLinkOrigin::Inherited,
            ..
        }
    ));
    assert_eq!(second.state("c1").active_link.unwrap().key, key(1));
}

// Given: a parent that relinked from one pull request to another / When: a child is derived /
// Then: the child is pinned to the link the parent has now
#[tokio::test(start_paused = true)]
async fn a_child_is_pinned_to_the_link_its_parent_has_when_it_is_derived() {
    let world = World::new();
    let mut process = world.process(enabled());
    linked_parent(&mut process, open_running(1)).await;
    process.source.push_ok(&key(2), open_running(2));
    process.link("t1", 2).await;

    let scheduled = process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();

    let InheritanceScheduling::Scheduled(row) = scheduled else {
        panic!("expected a pinned inheritance, got {scheduled:?}");
    };
    assert_eq!(row.key, key(2));
    assert_eq!(
        row.parent_link_id,
        process.state("t1").active_link.unwrap().link_id
    );
}

// Given: a lane, a parent without a link, and a repeated schedule / When: inheriting / Then: a
// lane never inherits, nothing is stored without a link, and a repeat changes nothing
#[tokio::test(start_paused = true)]
async fn lanes_and_unlinked_parents_inherit_nothing_and_repeats_are_harmless() {
    let world = World::new();
    let mut process = world.process(enabled());

    let without_link = process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    linked_parent(&mut process, open_running(1)).await;
    let lane = process
        .service
        .inherit("t1", "lane", DerivedThreadKind::Lane)
        .await
        .unwrap();
    let first = process
        .service
        .inherit("t1", "c2", DerivedThreadKind::Fork)
        .await
        .unwrap();
    let again = process
        .service
        .inherit("t1", "c2", DerivedThreadKind::Fork)
        .await
        .unwrap();

    assert_eq!(without_link, InheritanceScheduling::NoParentLink);
    assert_eq!(lane, InheritanceScheduling::Lane);
    assert!(
        process
            .storage_handle
            .pull_request_inheritance("lane")
            .unwrap()
            .is_none()
    );
    assert_eq!(first, again);
}

// Given: a pending inheritance / When: the child unlinks before it is resolved / Then: the
// decision stands: a later resolution does not bring the link back
#[tokio::test(start_paused = true)]
async fn an_unlink_before_the_resolution_is_never_undone() {
    let world = World::new();
    let mut process = world.process(enabled());
    linked_parent(&mut process, open_running(1)).await;
    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    let calls_before = process.source.calls().len();
    let permit = process.permit("c1");
    process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();

    process.service.unlink("c1", 0, &permit).await.unwrap();
    settle().await;
    let late = process.service.resolve_inheritance("c1", &permit).await;

    assert!(matches!(
        late,
        Ok(InheritanceResolution::AlreadyDecided(
            InheritanceState::Cancelled
        ))
    ));
    assert!(process.state("c1").active_link.is_none());
    assert_eq!(
        process.source.calls().len(),
        calls_before,
        "nothing was fetched for it"
    );
}

// Given: an inherited link / When: the parent unlinks / Then: the child keeps its link and its row
#[tokio::test(start_paused = true)]
async fn the_parent_unlinking_does_not_touch_the_child() {
    let world = World::new();
    let mut process = world.process(enabled());
    linked_parent(&mut process, open_running(1)).await;
    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    process.permit("c1");
    process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    next_for(&mut process, "c1").await;

    let permit = process.permit("t1");
    let revision = process.state("t1").lifecycle_revision;
    process
        .service
        .unlink("t1", revision, &permit)
        .await
        .unwrap();

    assert_eq!(process.state("c1").active_link.unwrap().key, key(1));
    assert_eq!(record(&process, "c1").state, InheritanceState::Applied);
}

// Given: a thread derived from a merged pull request / When: output names that pull request
// again, and another one / Then: only the other one is offered
#[tokio::test(start_paused = true)]
async fn no_candidate_is_offered_for_the_merged_pull_request_of_a_derived_thread() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.environment.bind("r2", "c1");
    let merged = fetched(1, facts(PullRequestState::Merged, passing()));
    linked_parent(&mut process, merged.clone()).await;
    process.source.push_ok(&key(1), merged);
    process.permit("c1");
    process
        .service
        .inherit("t1", "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    settle().await;
    assert_eq!(record(&process, "c1").state, InheritanceState::Skipped);

    for (call, number) in [("a", 1), ("b", 2)] {
        process.bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: call.into(),
            is_error: false,
            output: Some(format!("https://github.com/{REPOSITORY}/pull/{number}\n")),
            detail: None,
            run_id: Some("r2".into()),
            parent_call_id: None,
        }));
    }
    process.quiet_events().await;

    let candidates = process
        .storage_handle
        .pull_request_candidates("c1", None)
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].key, key(2));
}
