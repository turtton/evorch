//! Explicit refresh requests must outlive a fetch that was already in progress.

use super::super::flight::FlightOutcome;
use super::super::tests::{World, enabled, key, open_passing, titled};
use super::*;
use crate::pull_requests::FetchError;

// Drive rounds explicitly: the old flight must finish only after the scheduler has processed
// the refresh. Completion comes from the source gates and flights, not from time or yielding.
async fn refresh_during_a_fetch(failed: bool) {
    let world = World::new();
    let process = world.process(enabled());
    // Stop and join autonomous producers before manually driving rounds. Only
    // this fixture resets cancellation; a stopped production service stays stopped.
    process.service.shutdown_and_drain().await.unwrap();
    process.service.inner().shutdown.send_replace(false);
    process.link_directly("t1", 1);
    let permit = process.permit("t1");
    let inner = process.service.inner();
    let old_answer = process.source.push_gate(&key(1));
    let new_answer = process.source.push_gate(&key(1));

    inner.round().await;
    let first = inner.lock().flights[&key(1)].clone();
    old_answer.arrived().await;
    // Repeated button presses during one fetch coalesce into one follow-up, not concurrent I/O.
    for _ in 0..2 {
        process.service.refresh("t1", &permit).await.unwrap();
    }
    inner.round().await;
    assert_eq!(
        process.source.calls().len(),
        1,
        "the old fetch is still in flight"
    );

    if failed {
        old_answer.release_err(FetchError::not_found("old lookup failed"));
    } else {
        old_answer.release_ok(open_passing(1));
    }
    first.wait().await;
    inner.round().await;
    let second =
        inner.lock().flights.get(&key(1)).cloned().expect(
            "the explicit refresh must start a follow-up without waiting for polling/backoff",
        );
    assert!(!Arc::ptr_eq(&first, &second));
    new_answer.arrived().await;
    let mut updated = titled(open_passing(1), "new title");
    updated.facts.head_sha = "b".repeat(40);
    new_answer.release_ok(updated);
    assert!(matches!(
        second.wait().await,
        FlightOutcome::Committed { stored: true, .. }
    ));

    let snapshot = process
        .storage_handle
        .pull_request_snapshot(&key(1))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.facts.unwrap().head_sha, "b".repeat(40));
    assert_eq!(snapshot.display.title.unwrap().as_str(), "new title");
    inner.round().await;
    assert_eq!(
        process.source.calls().len(),
        2,
        "one follow-up serves both refresh requests"
    );
    assert!(inner.lock().flights.is_empty());
}

#[tokio::test(start_paused = true)]
async fn explicit_refresh_survives_an_in_flight_success() {
    refresh_during_a_fetch(false).await;
}

#[tokio::test(start_paused = true)]
async fn explicit_refresh_survives_an_in_flight_failure() {
    refresh_during_a_fetch(true).await;
}

#[tokio::test(start_paused = true)]
async fn repeated_producer_notifications_resume_one_pending_fetch_without_self_retriggering() {
    let world = World::new();
    let process = world.process(enabled());
    let _owner = process.permit("t1");
    let producers = process.service.producer_gate();
    let mut pause = producers.pause();
    process.link_directly("t1", 1);
    let answer = process.source.push_gate(&key(1));
    // Coalesced pause/resume notifications may arrive before the scheduler first polls.
    for _ in 0..8 {
        assert!(producers.resume(pause));
        pause = producers.pause();
    }
    assert!(process.source.calls().is_empty());
    assert!(producers.resume(pause));
    process.service.inner().wake.notify_one();
    answer.arrived().await;
    assert_eq!(process.source.calls().len(), 1);
    let flight = process.service.inner().lock().flights[&key(1)].clone();
    answer.release_ok(open_passing(1));
    flight.wait().await;
    process.service.inner().round().await;
    assert_eq!(process.source.calls().len(), 1);
    process.service.shutdown_and_drain().await.unwrap();
}
