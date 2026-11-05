//! Only the process that owns a thread acts for it (ADR 0024).

use storage::threads::ThreadUpsert;

use super::*;

/// An answer the old owner receives: its head is told apart from every other answer.
fn from_old_owner() -> FetchedPullRequest {
    let mut answer = open_running(1);
    answer.facts.head_sha = "b".repeat(40);
    answer
}

fn stored_head(process: &Process) -> String {
    process
        .storage_handle
        .pull_request_snapshot(&key(1))
        .unwrap()
        .and_then(|snapshot| snapshot.facts)
        .map(|facts| facts.head_sha)
        .unwrap_or_default()
}

// Given: a link in the database and nobody owning the thread, as after a restart / When:
// time passes, then a person refreshes (taking the lease the way chat does) / Then: nothing
// is fetched by itself, the refresh fetches, and the polling goes on
#[tokio::test(start_paused = true)]
async fn after_a_restart_a_refresh_makes_this_process_the_owner_and_polling_resumes() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    process.link_directly("t1", 1);

    pass(3_600).await;
    assert!(
        process.source.calls().is_empty(),
        "a process that does not own the thread fetches nothing"
    );

    let permit = process.permit("t1");
    process.service.refresh("t1", &permit).await.unwrap();
    assert!(matches!(
        process.next().await,
        PullRequestEvent::Changed { .. }
    ));
    assert_eq!(process.source.calls().len(), 1);
    pass(30).await;
    assert_eq!(process.source.calls().len(), 2, "the polling resumed");
}

// Given: an owner that acquires the thread later / When: the lease appears / Then: the
// ownership event alone makes the scheduler start (nothing polls on a timer for it)
#[tokio::test(start_paused = true)]
async fn an_ownership_change_wakes_the_scheduler() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.link_directly("t1", 1);
    settle().await;
    assert!(process.source.calls().is_empty());

    process.host.start("t1").unwrap();

    assert!(matches!(
        process.next().await,
        PullRequestEvent::Changed { .. }
    ));
}

// Given: two processes on one database, the thread owned by the first / When: the thread is
// linked and polled / Then: one fetch system serves it: the second process fetches nothing
// and publishes nothing, and the database numbers a single fetch
#[tokio::test(start_paused = true)]
async fn two_processes_showing_one_thread_fetch_and_notify_once() {
    let world = World::new();
    let mut owner = world.process(enabled());
    let mut viewer = world.process(enabled());
    owner.source.push_ok(&key(1), open_running(1));
    viewer.source.push_ok(&key(1), open_running(1));
    owner.source.repeat_last();
    viewer.source.repeat_last();

    owner.link("t1", 1).await;
    assert!(matches!(
        owner.next().await,
        PullRequestEvent::Linked { .. }
    ));
    assert!(matches!(
        owner.next().await,
        PullRequestEvent::Changed { .. }
    ));
    viewer.service.notify_changed();
    pass(30).await;

    assert!(viewer.source.calls().is_empty());
    assert!(viewer.quiet_events().await.is_empty());
    assert_eq!(owner.source.calls().len(), 2);
    let snapshot = owner
        .storage_handle
        .pull_request_snapshot(&key(1))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.fetch_seq, 2, "only the owner numbered fetches");
}

// Given: a fetch held while the lease is handed to another process / When: the fetch is
// released / Then: the old generation's answer is dropped (nothing stored, nothing
// published), and the new owner fetches by itself
#[tokio::test(start_paused = true)]
async fn an_answer_for_a_handed_off_generation_is_dropped() {
    let world = World::new();
    let mut first = world.process(enabled());
    let mut second = world.process(enabled());
    let gate = first.source.push_gate(&key(1));
    second.source.push_ok(&key(1), open_running(1));
    let permit = first.permit("t1");
    first.link("t1", 1).await;
    gate.arrived().await;

    first.host.handoff(&permit, &second.host).unwrap();
    gate.release_ok(from_old_owner());

    let events = first.quiet_events().await;
    assert!(only_changed(&events).is_empty(), "{events:?}");
    assert!(matches!(
        second.next().await,
        PullRequestEvent::Changed { .. }
    ));
    assert_eq!(
        stored_head(&first),
        "a".repeat(40),
        "only the new owner's answer is stored"
    );
    assert_eq!(
        first.source.calls().len(),
        1,
        "the old owner does not fetch again"
    );
    assert_eq!(second.source.calls().len(), 1);
}

// Given: a fetch held while the owner releases the thread / When: another process claims it and
// the fetch is released / Then: the released generation's answer is not used
#[tokio::test(start_paused = true)]
async fn an_answer_after_a_release_and_claim_is_not_used() {
    let world = World::new();
    let mut first = world.process(enabled());
    let mut second = world.process(enabled());
    let gate = first.source.push_gate(&key(1));
    second.source.push_ok(&key(1), open_running(1));
    first.link("t1", 1).await;
    gate.arrived().await;

    assert!(!first.host.quiesce().unwrap());
    let owner = second.host.attach("t1").unwrap();
    second.host.claim(&owner).unwrap();
    gate.release_ok(from_old_owner());

    let events = first.quiet_events().await;
    assert!(only_changed(&events).is_empty(), "{events:?}");
    assert!(matches!(
        second.next().await,
        PullRequestEvent::Changed { .. }
    ));
    assert_eq!(
        stored_head(&first),
        "a".repeat(40),
        "only the new owner's answer is stored"
    );
    assert_eq!(first.source.calls().len(), 1);
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

// Given: a permit for another thread, or a fenced one / When: a decision is made with it /
// Then: it is refused
#[tokio::test(start_paused = true)]
async fn a_decision_needs_the_lease_of_its_own_thread() {
    let world = World::new();
    let process = world.process(enabled());
    let other = process.permit("other");
    let mine = process.permit("t1");

    let wrong_thread = process
        .service
        .link("t1", &format!("{REPOSITORY}#1"), 0, &other)
        .await;
    let second = world.process(enabled());
    process.host.handoff(&mine, &second.host).unwrap();
    let fenced = process
        .service
        .link("t1", &format!("{REPOSITORY}#1"), 0, &mine)
        .await;

    assert!(matches!(wrong_thread, Err(PullRequestError::NotOwner)));
    assert!(matches!(fenced, Err(PullRequestError::NotOwner)));
    assert!(process.state("t1").active_link.is_none());
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

// Given: a permit that was checked, and a lease handed to another process after that check and
// before the write takes the registry guard / When: the candidate is ignored / Then: the write
// is refused, the candidate stays undecided and nothing is published
#[tokio::test(start_paused = true)]
async fn ignoring_a_candidate_after_the_lease_moved_writes_and_publishes_nothing() {
    let world = World::new();
    let mut first = world.process(enabled());
    let second = world.process(enabled());
    let permit = first.permit("t1");
    let candidate = offered(&first, 1);
    let (host, other, held) = (first.host.clone(), second.host.clone(), permit.clone());
    first.at_stage(Stage::Guarded, move || {
        host.handoff(&held, &other).unwrap();
    });

    let result = first
        .service
        .dismiss_candidate("t1", candidate.candidate_id, &permit)
        .await;

    assert!(
        matches!(result, Err(PullRequestError::NotOwner)),
        "{result:?}"
    );
    assert_eq!(
        candidate_state(&first, candidate.candidate_id),
        storage::pull_requests::CandidateState::Presented
    );
    assert!(first.quiet_events().await.is_empty());
}

// Given: the same move of the lease during a confirmation / Then: no link, no decision and no
// event: the decision's own event is published under the guard too
#[tokio::test(start_paused = true)]
async fn confirming_a_candidate_after_the_lease_moved_links_and_publishes_nothing() {
    let world = World::new();
    let mut first = world.process(enabled());
    let second = world.process(enabled());
    let permit = first.permit("t1");
    let candidate = offered(&first, 1);
    let (host, other, held) = (first.host.clone(), second.host.clone(), permit.clone());
    first.at_stage(Stage::Guarded, move || {
        host.handoff(&held, &other).unwrap();
    });

    let result = first
        .service
        .confirm_candidate("t1", candidate.candidate_id, 0, &permit)
        .await;

    assert!(
        matches!(result, Err(PullRequestError::NotOwner)),
        "{result:?}"
    );
    assert!(first.state("t1").active_link.is_none());
    assert_eq!(
        candidate_state(&first, candidate.candidate_id),
        storage::pull_requests::CandidateState::Presented
    );
    assert!(first.quiet_events().await.is_empty());
}

// Given: the lease still held / When: a candidate is ignored / Then: it is decided and the event
// follows the write
#[tokio::test(start_paused = true)]
async fn ignoring_a_candidate_while_holding_the_lease_decides_and_announces_it() {
    let world = World::new();
    let mut process = world.process(enabled());
    let permit = process.permit("t1");
    let candidate = offered(&process, 1);

    process
        .service
        .dismiss_candidate("t1", candidate.candidate_id, &permit)
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
