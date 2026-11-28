//! Which threads the service polls and decides for: every thread of the database.

use storage::threads::ThreadUpsert;

use super::*;

// Given: a link in the database that nobody told the service about, as after a restart /
// When: the scheduler's first round runs / Then: the pull request is fetched by itself and the
// polling goes on
#[tokio::test(start_paused = true)]
async fn after_a_restart_a_stored_link_is_polled_by_itself() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    process.link_directly("t1", 1);

    process.service.notify_changed();
    assert!(matches!(
        process.next().await,
        PullRequestEvent::Changed { .. }
    ));
    assert_eq!(process.source.calls().len(), 1);
    pass(30).await;
    assert_eq!(process.source.calls().len(), 2, "the polling goes on");
}

// Given: a tracked thread / When: it is archived / Then: its pull request is not polled
// any more
#[tokio::test(start_paused = true)]
async fn an_archived_thread_is_not_polled() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    process.link("t1", 1).await;
    process.next().await;
    process.next().await;
    assert_eq!(process.source.calls().len(), 1);

    process
        .storage_handle
        .upsert_threads_host(&[ThreadUpsert {
            thread_id: "t1".into(),
            archived: Some(true),
            archived_at_ns: Some(5),
            archive_revision: 1,
            created_at_ns: 1,
            updated_at_ns: 2,
            ..ThreadUpsert::default()
        }])
        .unwrap();
    process.service.notify_changed();
    pass(3_600).await;

    assert_eq!(process.source.calls().len(), 1);
    assert!(process.quiet_events().await.is_empty());
}

/// A candidate the thread was offered.
fn offered(process: &Process, number: u64) -> storage::pull_requests::PullRequestCandidate {
    process
        .storage_handle
        .present_pull_request_candidate(storage::pull_requests::NewPullRequestCandidate {
            thread_id: "t1".into(),
            key: key(number),
            source_run_id: "run".into(),
            source_call_id: "call".into(),
            at_ns: 1,
        })
        .unwrap()
        .candidate
}

fn candidate_state(process: &Process, id: u64) -> storage::pull_requests::CandidateState {
    process
        .storage_handle
        .pull_request_candidates("t1", None)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.candidate_id == id)
        .unwrap()
        .state
}

// Given: an offered candidate / When: it is ignored / Then: it is decided and the event
// follows the write
#[tokio::test(start_paused = true)]
async fn ignoring_a_candidate_decides_and_announces_it() {
    let world = World::new();
    let mut process = world.process(enabled());
    let candidate = offered(&process, 1);

    process
        .service
        .dismiss_candidate("t1", candidate.candidate_id)
        .await
        .unwrap();

    assert_eq!(
        candidate_state(&process, candidate.candidate_id),
        storage::pull_requests::CandidateState::Dismissed
    );
    let events = process.quiet_events().await;
    assert!(
        matches!(
            events.as_slice(),
            [PullRequestEvent::CandidateChanged {
                state: event_bus::PullRequestCandidateState::Dismissed,
                ..
            }]
        ),
        "{events:?}"
    );
}
