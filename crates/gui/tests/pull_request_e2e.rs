//! ADR 0043 Phase 1 (PR6): scenarios that cross crates. The pull request service, storage, the
//! owner registry, the event bus and the GUI's window state run together; only the pull request
//! source is scripted.
//!
//! Time is virtual (`start_paused`), so the pending/stable/backoff intervals are crossed with
//! `advance` and nothing sleeps. Every wait is for an event the service published or for a gate
//! the scripted source holds. Blocking storage calls keep the paused clock from auto-advancing
//! while they are outstanding. "Nothing more was fetched" is shown after the scheduler went
//! through a full round (`scheduler_round`), which plans every key that is due.

use std::sync::Arc;

use event_bus::{
    Event, EventKind, PullRequestEvent, PullRequestFetchStatus, PullRequestLinkOrigin,
};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::notifications::NotificationKind;
use gui::model::pull_requests::{PrGlyph, PullRequestAction, chips_by_call, glyph};
use runtime::pull_requests::{DerivedThreadKind, FetchError};
use storage::pull_requests::{
    CandidateState, InheritanceResolution, InheritanceState, PullRequestState,
};
use workspace_ui::ThreadId;

#[path = "support/short_private_directory.rs"]
mod short_private_directory;
use short_private_directory::ShortPrivateDirectory;

#[path = "support/pull_request_world.rs"]
mod pull_request_world;
use pull_request_world::{
    CreatePullRequestModel, Process, THREAD, changed, enabled, facts, failing, fetched, key,
    link_action, merged, pass, passing, running, settle, sha, window,
};

fn linked(thread: &'static str) -> impl Fn(&PullRequestEvent) -> bool {
    move |event| matches!(event, PullRequestEvent::Linked { thread_id, .. } if thread_id == thread)
}

fn unlinked(thread: &'static str) -> impl Fn(&PullRequestEvent) -> bool {
    move |event| matches!(event, PullRequestEvent::Unlinked { thread_id, .. } if thread_id == thread)
}

fn refresh(thread: &str) -> PullRequestAction {
    PullRequestAction::Refresh {
        thread_id: thread.into(),
    }
}

fn unlink(thread: &str) -> PullRequestAction {
    PullRequestAction::Unlink {
        thread_id: thread.into(),
    }
}

fn kinds(state: &WorkbenchState<DemoSource>) -> Vec<NotificationKind> {
    state
        .notifications()
        .items()
        .map(|item| item.kind.clone())
        .filter(|kind| format!("{kind:?}").starts_with("PullRequest"))
        .collect()
}

fn fetch_statuses(events: &[Event]) -> Vec<PullRequestFetchStatus> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::PullRequest(PullRequestEvent::FetchStatusChanged {
                thread_id,
                status,
                ..
            }) if thread_id == THREAD => Some(status.clone()),
            _ => None,
        })
        .collect()
}

/// The diagnostics the pull request service could be raising a failure through: any whose source
/// or code names pull requests, or that belongs to `thread`. The marker the tests publish is not
/// one of them.
///
/// ADR 0043 reports a failing source through `FetchStatusChanged` (PR3's decision), which the
/// scenario below counts, and the service publishes no `Diagnostic` event of its own; the
/// count of these stays at zero, at the first failure and after it has repeated.
fn pull_request_diagnostics(events: &[Event], thread: &str) -> usize {
    events
        .iter()
        .filter(|event| match &event.kind {
            EventKind::Diagnostic(diagnostic) => {
                let marker = diagnostic.source == "pull-request-e2e" && diagnostic.code == "marker";
                !marker
                    && (diagnostic.source.contains("pull_request")
                        || diagnostic.source.contains("pull-request")
                        || diagnostic.code.contains("pull_request")
                        || diagnostic.thread_id.as_deref() == Some(thread))
            }
            _ => false,
        })
        .count()
}

fn pull_request_events_of(events: &[Event], thread: &str) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(&event.kind, EventKind::PullRequest(change) if change.thread_id() == thread)
        })
        .count()
}

struct Site {
    directory: tempfile::TempDir,
    owners: ShortPrivateDirectory,
}

impl Site {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
            owners: ShortPrivateDirectory::new(),
        }
    }

    fn open(&self, model: Arc<dyn runtime::AgentModel>) -> Process {
        Process::open(
            &self.directory.path().join("e2e.db"),
            self.owners.path(),
            model,
            enabled(),
        )
    }

    fn window(&self, process: &Process, threads: &[&str]) -> WorkbenchState<DemoSource> {
        let mut window = window(
            &process.config.db_path,
            self.directory.path(),
            &process.host,
            process.sink(),
            threads,
        );
        settle(&mut window);
        window
    }
}

// Scenario 1. Given: a tool output that names a pull request / When: the person confirms the
// chip, CI fails and then the pull request is merged / Then: each notification is raised once,
// the merge stops the tracking, and no fetch follows however much time passes
#[tokio::test(start_paused = true)]
async fn a_detected_candidate_runs_to_a_merge_and_is_then_left_alone() {
    let site = Site::new();
    let mut process = site.open(CreatePullRequestModel::new([12]));
    process.host.start(THREAD).unwrap();
    let mut window = site.window(&process, &[THREAD]);
    process.source.push_ok(&key(12), running(12));
    process.source.push_ok(&key(12), failing(12, 'b'));
    process.source.push_ok(&key(12), merged(12));

    // When: the agent's shell prints a pull request URL.
    process.run_agent("open the pull request").await;
    process
        .until(|event| matches!(event, PullRequestEvent::CandidateChanged { .. }))
        .await;
    process.show(&mut window);

    // Then: the chip offers it; nothing is linked or fetched yet.
    let view = window.pull_request_model().view(THREAD).unwrap();
    let chips = chips_by_call(view);
    let offered: Vec<_> = chips.values().flatten().collect();
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].label, "turtton/evorch#12");
    assert!(process.linked_to(THREAD).is_none());
    assert_eq!(process.fetches_of(12), 0);

    // When: the chip's Link PR is pressed.
    let candidate = process
        .database()
        .pull_request_candidates(THREAD, Some(CandidateState::Presented))
        .unwrap()[0]
        .candidate_id;
    window.apply_pull_request_action(PullRequestAction::Confirm {
        thread_id: THREAD.into(),
        candidate_id: candidate,
    });
    process.until(changed(THREAD)).await;
    process.show(&mut window);

    // Then: the pull request is pending and nothing was notified.
    let view = window.pull_request_model().linked(THREAD).unwrap();
    assert_eq!(glyph(view), Some(PrGlyph::Pending));
    assert_eq!(kinds(&window), [], "no notification yet");

    // When: CI fails (the same events reach the window twice).
    pass(30).await;
    process.until(changed(THREAD)).await;
    let failed = process.seen.clone();
    process.show(&mut window);
    window.apply_events(failed);
    settle(&mut window);

    // Then: one notification.
    let thread = ThreadId::new(THREAD);
    assert_eq!(
        kinds(&window),
        [NotificationKind::PullRequestChecksFailed {
            thread_id: thread.clone()
        }]
    );

    // When: the pull request is merged at the next (stable) poll.
    pass(300).await;
    process.until(changed(THREAD)).await;
    process.show(&mut window);

    // Then: the merge is notified once, after the failure.
    assert_eq!(
        kinds(&window),
        [
            NotificationKind::PullRequestChecksFailed {
                thread_id: thread.clone()
            },
            NotificationKind::PullRequestMerged { thread_id: thread }
        ]
    );
    assert_eq!(
        glyph(window.pull_request_model().linked(THREAD).unwrap()),
        Some(PrGlyph::Merged)
    );
    assert_eq!(process.fetches_of(12), 3);

    // And: a day later it has not been fetched again, and nothing more is raised.
    pass(86_400).await;
    process.scheduler_round().await;
    assert_eq!(
        process.fetches_of(12),
        3,
        "a merged pull request is left alone"
    );
    process.show(&mut window);
    assert_eq!(kinds(&window).len(), 2);
}

// Scenario 2. Given: a thread linked and unlinked while a fetch for it was out / When:
// a replacement opens before the old process stops and the late answer arrives / Then: the
// thread stays unlinked, the review is given no facts, and nothing is published or fetched for it
#[tokio::test(start_paused = true)]
async fn an_unlinked_thread_stays_unlinked_through_an_overlapping_restart_and_a_late_answer() {
    let site = Site::new();
    let model = || CreatePullRequestModel::new([]);
    let mut first = site.open(model());
    first.host.start(THREAD).unwrap();
    let mut before = site.window(&first, &[THREAD]);
    let gate = first.source.push_gate(&key(12));
    before.apply_pull_request_action(link_action("turtton/evorch#12"));
    first.until(linked(THREAD)).await;
    gate.arrived().await;
    first.show(&mut before);
    assert!(before.pull_request_model().linked(THREAD).is_some());

    // When: the person unlinks while the fetch is out, and the replacement opens.
    before.apply_pull_request_action(unlink(THREAD));
    first.until(unlinked(THREAD)).await;
    first.show(&mut before);
    assert!(before.pull_request_model().linked(THREAD).is_none());
    // The old service stays alive until its late answer below. Once shutdown
    // starts, fetch cancellation is required; that boundary is tested separately.
    let mut second = site.open(model());
    let mut after = site.window(&second, &[THREAD]);

    // Then: the reopened database has no link, no bar, and the review has no facts.
    assert!(second.linked_to(THREAD).is_none());
    assert!(after.pull_request_model().linked(THREAD).is_none());
    let run = second
        .runtime
        .delegate_chat(
            THREAD,
            runtime::Role::Orchestrator,
            "hello".into(),
            runtime::RunConfig::default(),
        )
        .unwrap();
    assert_eq!(
        second.runtime.wait(run).await,
        Ok(event_bus::AgentRunPhase::Done)
    );
    let lookup = |process: &Process| {
        let (service, run) = (process.service.clone(), run.to_string());
        async move { service.snapshot_for_review(&run).await.unwrap() }
    };
    assert!(matches!(
        lookup(&second).await,
        runtime::pull_requests::ReviewLookup::NotLinked
    ));

    // When: the old process's fetch finally answers.
    gate.release_ok(running(12));
    second.facts_stored(12).await;
    settle(&mut after);

    // Then: the answer is stored for the pull request but revives nothing.
    assert!(second.linked_to(THREAD).is_none());
    assert!(after.pull_request_model().linked(THREAD).is_none());
    assert!(matches!(
        lookup(&second).await,
        runtime::pull_requests::ReviewLookup::NotLinked
    ));
    let published = first.quiet_events().await;
    assert_eq!(
        pull_request_events_of(&published, THREAD),
        0,
        "the unlinked thread hears nothing of the late answer"
    );
    pass(3_600).await;
    second.scheduler_round().await;
    assert_eq!(second.fetches_of(12), 0);
    assert!(second.linked_to(THREAD).is_none());
    first.service.shutdown_and_drain().await.unwrap();
    second.service.shutdown_and_drain().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_drains_the_unlinked_fetch_before_a_replacement_opens() {
    let site = Site::new();
    let mut first = site.open(CreatePullRequestModel::new([]));
    first.host.start(THREAD).unwrap();
    let mut before = site.window(&first, &[THREAD]);
    let gate = first.source.push_gate(&key(12));
    before.apply_pull_request_action(link_action("turtton/evorch#12"));
    first.until(linked(THREAD)).await;
    gate.arrived().await;
    first.show(&mut before);
    before.apply_pull_request_action(unlink(THREAD));
    first.until(unlinked(THREAD)).await;

    // Drain is the completion signal: even an answer supplied afterwards cannot
    // commit facts. No timer or absence-of-events wait stands in for completion.
    first.service.shutdown_and_drain().await.unwrap();
    gate.release_ok(running(12));
    let mut second = site.open(CreatePullRequestModel::new([]));
    assert!(second.linked_to(THREAD).is_none());
    assert!(
        second
            .database()
            .pull_request_snapshot(&key(12))
            .unwrap()
            .and_then(|snapshot| snapshot.facts)
            .is_none()
    );
    let published = first.quiet_events().await;
    assert_eq!(pull_request_events_of(&published, THREAD), 0);
    second.scheduler_round().await;
    assert_eq!(second.fetches_of(12), 0);
    second.service.shutdown_and_drain().await.unwrap();
}

// Scenario 3. Given: two derived threads whose inheritance is still pending / When: the
// process restarts and the owners take the threads / Then: the one the person did not touch
// takes the link once the pull request is confirmed open; the one that linked and unlinked on
// its own never gets the parent's link back
#[tokio::test(start_paused = true)]
async fn a_pending_inheritance_is_decided_after_a_restart_and_never_overrides_the_child() {
    let site = Site::new();
    let model = || CreatePullRequestModel::new([]);
    let mut first = site.open(model());
    first.source.push_ok(&key(12), running(12));
    first.source.repeat_last();
    let parent = first.permit(THREAD);
    first
        .service
        .link(THREAD, "turtton/evorch#12", 0, &parent)
        .await
        .unwrap();
    first.until(changed(THREAD)).await;
    for child in ["c1", "c2"] {
        first
            .service
            .inherit(THREAD, child, DerivedThreadKind::Escalation)
            .await
            .unwrap();
    }
    pass(3_600).await;
    for child in ["c1", "c2"] {
        let record = first
            .storage
            .handle()
            .pull_request_inheritance(child)
            .unwrap()
            .unwrap();
        assert_eq!(
            record.state,
            InheritanceState::Pending,
            "{child} has no owner"
        );
    }
    assert!(first.linked_to("c1").is_none());

    // When: the process restarts. The first child to be owned links and unlinks by itself
    // while the confirming fetch is out.
    first.service.shutdown();
    let mut second = site.open(model());
    let mut window = site.window(&second, &[THREAD, "c1", "c2"]);
    let gate = second.source.push_gate(&key(12));
    second.source.push_ok(&key(12), running(12));
    second.source.repeat_last();
    second.source.push_ok(&key(13), running(13));
    let c2 = second.permit("c2");
    second.service.notify_changed();
    gate.arrived().await;
    second
        .service
        .link("c2", "turtton/evorch#13", second.revision("c2"), &c2)
        .await
        .unwrap();
    second
        .service
        .unlink("c2", second.revision("c2"), &c2)
        .await
        .unwrap();
    gate.release_ok(running(12));
    let resolution = second.service.resolve_inheritance("c2", &c2).await;

    // Then: the confirmation does not bring the parent's link back to c2.
    assert!(
        !matches!(resolution, Ok(InheritanceResolution::Applied { .. })),
        "{resolution:?}"
    );
    assert!(second.linked_to("c2").is_none());

    // When: the other child is owned.
    second.permit("c1");
    second.service.notify_changed();
    second
        .until(|event| {
            matches!(
                event,
                PullRequestEvent::Linked { thread_id, origin: PullRequestLinkOrigin::Inherited, .. }
                    if thread_id == "c1"
            )
        })
        .await;
    second.show(&mut window);

    // Then: it took the link after the pull request was confirmed open, and c2 still has none.
    assert_eq!(second.linked_to("c1"), Some(key(12)));
    assert!(window.pull_request_model().linked("c1").is_some());
    assert!(window.pull_request_model().linked("c2").is_none());
    assert!(second.linked_to("c2").is_none());
}

// Scenario 4a. Given: one thread opened by two processes (one registry directory, one
// database) / When: time passes and CI fails / Then: only the owner fetches and notifies; the
// other shows the stored facts and publishes nothing
#[tokio::test(start_paused = true)]
async fn only_the_owning_process_fetches_and_notifies_for_a_shared_thread() {
    let site = Site::new();
    let mut owner = site.open(CreatePullRequestModel::new([]));
    let mut viewer = site.open(CreatePullRequestModel::new([]));
    owner.host.start(THREAD).unwrap();
    assert!(
        viewer.host.owned_permit(THREAD).is_err(),
        "the second process does not hold the thread"
    );
    let mut owning = site.window(&owner, &[THREAD]);
    let mut viewing = site.window(&viewer, &[THREAD]);
    owner.source.push_ok(&key(12), running(12));
    owner.source.push_ok(&key(12), failing(12, 'b'));
    owner.source.repeat_last();
    viewer.source.push_ok(&key(12), failing(12, 'z'));
    viewer.source.repeat_last();

    // When: the owner links and its events reach both windows (as the shared feed delivers them).
    owning.apply_pull_request_action(link_action("turtton/evorch#12"));
    owner.until(changed(THREAD)).await;
    let shared = owner.seen.clone();
    owner.show(&mut owning);
    viewing.apply_events(shared);
    settle(&mut viewing);
    pass(30).await;
    owner.until(changed(THREAD)).await;
    let shared = owner.seen.clone();
    owner.show(&mut owning);
    viewing.apply_events(shared);
    settle(&mut viewing);
    viewer.scheduler_round().await;

    // Then: one fetch line, one notification, and the viewer only shows.
    assert_eq!(owner.fetches_of(12), 2);
    assert_eq!(viewer.fetches_of(12), 0);
    assert_eq!(
        kinds(&owning),
        [NotificationKind::PullRequestChecksFailed {
            thread_id: ThreadId::new(THREAD)
        }]
    );
    assert!(kinds(&viewing).is_empty());
    let view = viewing.pull_request_model().linked(THREAD).unwrap();
    assert!(!view.tracked_here);
    assert_eq!(glyph(view), Some(PrGlyph::Failing));
    let heard = viewer.quiet_events().await;
    assert_eq!(pull_request_events_of(&heard, THREAD), 0);
}

// Scenario 4b. Given: two threads in two processes tracking one pull request, the first fetch
// (numbered earlier) held back / When: the second process stores newer facts and then the held
// answer arrives / Then: the stored facts do not go back to the older answer
#[tokio::test(start_paused = true)]
async fn a_late_older_answer_does_not_roll_a_shared_snapshot_back() {
    let site = Site::new();
    let slow = site.open(CreatePullRequestModel::new([]));
    let mut fast = site.open(CreatePullRequestModel::new([]));
    let held = slow.source.push_gate(&key(20));
    let slow_permit = slow.permit("t-slow");
    slow.service
        .link("t-slow", "turtton/evorch#20", 0, &slow_permit)
        .await
        .unwrap();
    held.arrived().await;

    // When: the other process, which began later, stores newer facts first.
    fast.source.push_ok(
        &key(20),
        fetched(20, facts(PullRequestState::Open, 'b', Vec::new())),
    );
    let fast_permit = fast.permit("t-fast");
    fast.service
        .link("t-fast", "turtton/evorch#20", 0, &fast_permit)
        .await
        .unwrap();
    fast.until(changed("t-fast")).await;
    assert_eq!(fast.facts_stored(20).await.head_sha, sha('b'));

    // And: the older answer arrives afterwards.
    held.release_ok(fetched(20, facts(PullRequestState::Open, 'a', Vec::new())));

    // The refused answer publishes nothing, so the next poll of the slow process (held at its
    // source) shows that the late answer has been dealt with: one flight runs at a time.
    let next = slow.source.push_gate(&key(20));
    while slow.fetches_of(20) < 2 {
        pass(300).await;
    }
    next.arrived().await;

    // Then: the stored snapshot is still the newer one.
    assert_eq!(fast.facts_stored(20).await.head_sha, sha('b'));
    assert_eq!(slow.linked_to("t-slow"), Some(key(20)));
    next.release_ok(fetched(20, facts(PullRequestState::Open, 'b', Vec::new())));
}

// Scenario 5a. Given: a child that took its parent's link / When: the parent unlinks / Then: the
// child keeps the link, is still shown, and is still polled
#[tokio::test(start_paused = true)]
async fn unlinking_the_parent_leaves_the_child_tracked() {
    let site = Site::new();
    let mut process = site.open(CreatePullRequestModel::new([]));
    process.host.start(THREAD).unwrap();
    let mut window = site.window(&process, &[THREAD, "c1"]);
    process.source.push_ok(&key(12), running(12));
    process.source.push_ok(&key(12), passing(12));
    process.source.repeat_last();
    window.apply_pull_request_action(link_action("turtton/evorch#12"));
    process.until(changed(THREAD)).await;
    process
        .service
        .inherit(THREAD, "c1", DerivedThreadKind::Escalation)
        .await
        .unwrap();
    process.permit("c1");
    process.service.notify_changed();
    process.until(linked("c1")).await;
    process.show(&mut window);

    // When: the parent unlinks.
    window.apply_pull_request_action(unlink(THREAD));
    process.until(unlinked(THREAD)).await;
    process.show(&mut window);

    // Then: the parent has no bar, the child has one, and time still brings fetches for it.
    assert!(window.pull_request_model().linked(THREAD).is_none());
    assert!(window.pull_request_model().linked("c1").is_some());
    assert_eq!(process.linked_to("c1"), Some(key(12)));
    let before = process.fetches_of(12);
    process.advance_until_fetches(12, before + 1, 30).await;
    assert!(process.linked_to(THREAD).is_none());
}

// Scenario 5b. Given: parents whose pull requests are closed and merged / When: threads derived
// from them are owned / Then: neither takes the link; both decisions are stored as skipped
#[tokio::test(start_paused = true)]
async fn a_closed_or_merged_pull_request_is_not_inherited() {
    let site = Site::new();
    let mut process = site.open(CreatePullRequestModel::new([]));
    process.source.push_ok(
        &key(13),
        fetched(13, facts(PullRequestState::Closed, 'a', Vec::new())),
    );
    process.source.repeat_last();
    process.source.push_ok(&key(14), merged(14));
    process.source.repeat_last();
    for (parent, number) in [("closed-parent", 13), ("merged-parent", 14)] {
        let permit = process.permit(parent);
        process
            .service
            .link(parent, &format!("turtton/evorch#{number}"), 0, &permit)
            .await
            .unwrap();
        process.until(changed(parent)).await;
    }

    for (parent, child) in [
        ("closed-parent", "closed-child"),
        ("merged-parent", "merged-child"),
    ] {
        process
            .service
            .inherit(parent, child, DerivedThreadKind::Escalation)
            .await
            .unwrap();
        let permit = process.permit(child);
        let resolution = process.service.resolve_inheritance(child, &permit).await;
        assert!(
            !matches!(resolution, Ok(InheritanceResolution::Applied { .. })),
            "{child}: {resolution:?}"
        );
        assert!(process.linked_to(child).is_none(), "{child}");
        let record = process
            .storage
            .handle()
            .pull_request_inheritance(child)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, InheritanceState::Skipped, "{child}");
    }
}

// Scenario 6. Given: gh is not signed in / When: the fetch fails again and again / Then: the
// retries back off and the problem is published once; a Refresh after signing in recovers it;
// a closed pull request stops the polling and a Refresh that finds it open resumes it
#[tokio::test(start_paused = true)]
async fn an_auth_failure_backs_off_quietly_and_refresh_recovers_and_reopens() {
    let site = Site::new();
    let mut process = site.open(CreatePullRequestModel::new([]));
    process.host.start(THREAD).unwrap();
    let mut window = site.window(&process, &[THREAD]);
    // The first fetch and three retries fail; the script then holds what gh answers once it is
    // signed in, then a closed pull request, then an open one that is polled once more.
    for _ in 0..4 {
        process.source.push_err(
            &key(12),
            FetchError::Unauthenticated("gh auth login".into()),
        );
    }
    process.source.push_ok(&key(12), running(12));
    process.source.push_ok(
        &key(12),
        fetched(12, facts(PullRequestState::Closed, 'a', Vec::new())),
    );
    process.source.push_ok(&key(12), running(12));
    process.source.push_ok(&key(12), running(12));

    // When: the link is made and the retries run.
    window.apply_pull_request_action(link_action("turtton/evorch#12"));
    process.until(linked(THREAD)).await;
    process
        .until(|event| matches!(event, PullRequestEvent::FetchStatusChanged { .. }))
        .await;
    let first_report: Vec<Event> = process.seen.clone();
    assert_eq!(process.fetches_of(12), 1);
    process.scheduler_round().await;
    assert_eq!(process.fetches_of(12), 1, "no retry before time passes");
    for attempt in 2..=4 {
        process.advance_until_fetches(12, attempt, 30).await;
    }

    // Then: one report of the problem, nothing for the person to be told twice.
    let heard: Vec<Event> = process.seen.clone();
    let rest = process.quiet_events().await;
    let all: Vec<Event> = heard.into_iter().chain(rest).collect();
    assert_eq!(fetch_statuses(&all).len(), 1, "{:?}", fetch_statuses(&all));
    assert_eq!(
        fetch_statuses(&first_report),
        fetch_statuses(&all),
        "the failure repeated three times and was reported no more than at the first"
    );
    assert_eq!(pull_request_diagnostics(&first_report, THREAD), 0);
    assert_eq!(
        pull_request_diagnostics(&all, THREAD),
        pull_request_diagnostics(&first_report, THREAD),
        "repeating the failure raised no diagnostic"
    );
    process.show(&mut window);
    assert!(kinds(&window).is_empty());

    // When: gh is signed in and the person presses Refresh.
    window.apply_pull_request_action(refresh(THREAD));
    process.until(changed(THREAD)).await;
    let recovered = process.seen.clone();
    process.show(&mut window);

    // Then: the recovery is reported and the facts are shown.
    assert_eq!(
        fetch_statuses(&recovered),
        [PullRequestFetchStatus::Recovered]
    );
    assert_eq!(
        glyph(window.pull_request_model().linked(THREAD).unwrap()),
        Some(PrGlyph::Pending)
    );

    // When: the pull request is closed, the polling stops; a Refresh finds it open again.
    window.apply_pull_request_action(refresh(THREAD));
    process.until(changed(THREAD)).await;
    process.show(&mut window);
    let closed_at = process.fetches_of(12);
    pass(86_400).await;
    process.scheduler_round().await;
    assert_eq!(
        process.fetches_of(12),
        closed_at,
        "a closed pull request is left alone"
    );
    window.apply_pull_request_action(refresh(THREAD));
    process.until(changed(THREAD)).await;
    process.show(&mut window);

    // Then: it is open again and polled again.
    assert_eq!(
        glyph(window.pull_request_model().linked(THREAD).unwrap()),
        Some(PrGlyph::Pending)
    );
    let reopened_at = process.fetches_of(12);
    process.advance_until_fetches(12, reopened_at + 1, 30).await;
}
