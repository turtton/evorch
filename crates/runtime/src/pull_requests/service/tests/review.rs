//! What the escalation review reads, and the bounded refresh before it.

use super::*;
use crate::pull_requests::review::{Freshness, ReviewLookup, ReviewRefresh, StaleReason};

fn linked(lookup: ReviewLookup) -> Box<crate::pull_requests::ReviewPullRequest> {
    match lookup {
        ReviewLookup::Linked(review) => review,
        other => panic!("expected a linked pull request, got {other:?}"),
    }
}

/// An answer fetched at the clock's current time.
fn fetched_now(world: &World, number: u64) -> FetchedPullRequest {
    let mut answer = open_passing(number);
    answer.fetched_at_ns = world.clock.0.load(Ordering::SeqCst);
    answer
}

async fn tracked(world: &World, config: PullRequestsConfig) -> Process {
    let mut process = world.process(config);
    process.environment.bind("r1", "t1");
    process.source.push_ok(&key(1), fetched_now(world, 1));
    process.link("t1", 1).await;
    process.next().await;
    process.next().await;
    process
}

// Given: tracking off, review input off, and a run that is in no thread / When: the review
// looks / Then: each case says so and no pull request item would be built
#[tokio::test(start_paused = true)]
async fn the_review_sees_nothing_when_tracking_or_inclusion_is_off_or_the_thread_is_unknown() {
    let world = World::new();
    let process = world.process(PullRequestsConfig::default());
    process.environment.bind("r1", "t1");
    assert_eq!(
        process.service.snapshot_for_review("r1").await.unwrap(),
        ReviewLookup::Disabled
    );

    process
        .service
        .update_settings(PullRequestsConfig {
            include_in_review: false,
            ..enabled()
        })
        .unwrap();
    assert_eq!(
        process.service.snapshot_for_review("r1").await.unwrap(),
        ReviewLookup::NotIncluded
    );

    process.service.update_settings(enabled()).unwrap();
    assert_eq!(
        process.service.snapshot_for_review("nobody").await.unwrap(),
        ReviewLookup::NotLinked
    );
    assert_eq!(
        process.service.snapshot_for_review("r1").await.unwrap(),
        ReviewLookup::NotLinked,
        "a thread that never linked has no item"
    );
}

// Given: a tracked thread / When: the review reads, then the facts age, then an Unlink happens /
// Then: it is fresh, then stale because too old, and after the Unlink the item is gone
#[tokio::test(start_paused = true)]
async fn the_review_reads_stored_facts_with_their_freshness() {
    let world = World::new();
    let process = tracked(&world, enabled()).await;

    let fresh = linked(process.service.snapshot_for_review("r1").await.unwrap());
    world.clock.advance(61);
    let old = linked(process.service.snapshot_for_review("r1").await.unwrap());
    let permit = process.permit("t1");
    let revision = process.state("t1").lifecycle_revision;
    process
        .service
        .unlink("t1", revision, &permit)
        .await
        .unwrap();
    let gone = process.service.snapshot_for_review("r1").await.unwrap();

    assert_eq!(fresh.freshness, Freshness::Fresh);
    assert_eq!(fresh.facts.as_ref().unwrap().state, PullRequestState::Open);
    assert_eq!(old.freshness, Freshness::Stale(StaleReason::TooOld));
    assert_eq!(gone, ReviewLookup::NotLinked);
}

// Given: facts that are recent / When: the review refreshes / Then: nothing is fetched
#[tokio::test(start_paused = true)]
async fn recent_facts_are_used_without_a_fetch() {
    let world = World::new();
    let process = tracked(&world, enabled()).await;
    let calls = process.source.calls().len();

    let supply = process.service.refresh_for_review("r1").await.unwrap();

    assert_eq!(supply.refresh, ReviewRefresh::NotNeeded);
    assert_eq!(process.source.calls().len(), calls);
}

// Given: facts older than the limit and an owned thread / When: the review refreshes / Then: a
// short fetch runs with the review's deadline and the result is fresh
#[tokio::test(start_paused = true)]
async fn old_facts_are_refreshed_for_the_review() {
    let world = World::new();
    let process = tracked(&world, enabled()).await;
    world.clock.advance(61);
    process.source.push_ok(&key(1), fetched_now(&world, 1));

    let supply = process.service.refresh_for_review("r1").await.unwrap();

    assert_eq!(supply.refresh, ReviewRefresh::Refreshed);
    assert_eq!(linked(supply.lookup).freshness, Freshness::Fresh);
    assert_eq!(process.source.calls().len(), 2);
}

// Given: a fetch that takes longer than the review may wait / When: the review refreshes /
// Then: it proceeds with the stored facts marked stale, and the fetch keeps going for the tracker
#[tokio::test(start_paused = true)]
async fn a_slow_refresh_does_not_hold_the_review_and_is_not_cancelled() {
    let world = World::new();
    let process = tracked(&world, enabled()).await;
    world.clock.advance(61);
    let gate = process.source.push_gate(&key(1));

    let supply = process.service.refresh_for_review("r1").await.unwrap();

    assert_eq!(supply.refresh, ReviewRefresh::TimedOut);
    assert_eq!(
        linked(supply.lookup).freshness,
        Freshness::Stale(StaleReason::TooOld)
    );
    let mut answer = fetched_now(&world, 1);
    answer.facts.head_sha = "c".repeat(40);
    gate.release_ok(answer);
    settle().await;
    let stored = linked(process.service.snapshot_for_review("r1").await.unwrap());
    assert_eq!(stored.facts.unwrap().head_sha, "c".repeat(40));
}

// Given: a failing fetch / When: the review refreshes / Then: it reports the failure and the
// facts it hands over are stale
#[tokio::test(start_paused = true)]
async fn a_failed_refresh_hands_over_stale_facts() {
    let world = World::new();
    let process = tracked(&world, enabled()).await;
    world.clock.advance(61);
    process.source.push_err(
        &key(1),
        crate::pull_requests::FetchError::Network("down".into()),
    );

    let supply = process.service.refresh_for_review("r1").await.unwrap();

    assert_eq!(supply.refresh, ReviewRefresh::FetchFailed);
    let review = linked(supply.lookup);
    assert_eq!(
        review.freshness,
        Freshness::Stale(StaleReason::LastFetchFailed)
    );
    assert!(review.last_failure.is_some());
    assert!(
        review.facts.is_some(),
        "the last good facts are still there"
    );
}

// Given: a thread this process does not own / When: the review refreshes / Then: nothing is
// fetched and the facts are handed over as stored
#[tokio::test(start_paused = true)]
async fn a_process_that_does_not_own_the_thread_does_not_fetch_for_the_review() {
    let world = World::new();
    let process = world.process(enabled());
    process.environment.bind("r1", "t1");
    process.link_directly("t1", 1);

    let supply = process.service.refresh_for_review("r1").await.unwrap();

    assert_eq!(supply.refresh, ReviewRefresh::NotOwner);
    assert_eq!(
        linked(supply.lookup).freshness,
        Freshness::Stale(StaleReason::NoFacts)
    );
    assert!(process.source.calls().is_empty());
}
