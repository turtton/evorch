//! Polling, backoff, events and the races of a fetch with Unlink and Link.

use event_bus::{
    PullRequestChangedField, PullRequestEvent, PullRequestFetchStatus, PullRequestLinkEnd,
};
use storage::pull_requests::{NotificationKey, PullRequestState};

use super::*;
use crate::pull_requests::FetchError;

fn changed(event: &PullRequestEvent) -> (&[PullRequestChangedField], &[String], u64) {
    match event {
        PullRequestEvent::Changed {
            fields,
            notifications,
            transition_seq,
            ..
        } => (fields, notifications, *transition_seq),
        other => panic!("expected a change, got {other:?}"),
    }
}

/// Git for building a checkout: no global or system configuration.
fn git(dir: &std::path::Path, arguments: &[&str]) -> String {
    let mut command = std::process::Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    let output = command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(arguments)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {arguments:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

// Given: a tracked thread whose checkout is on the pull request's branch / When: the checkout
// moves to another branch / Then: the old commit is forgotten and the change is published
#[tokio::test(start_paused = true)]
async fn a_checkout_that_can_no_longer_be_named_forgets_the_old_local_head() {
    let world = World::new();
    let mut process = world.process(enabled());
    let checkout = world.dir.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "--quiet", "--initial-branch", "feat"]);
    std::fs::write(checkout.join("file.txt"), "x").unwrap();
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "--quiet", "-m", "initial"]);
    git(
        &checkout,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/turtton/evorch.git",
        ],
    );
    process
        .environment
        .set_directories(vec![std::fs::canonicalize(&checkout).unwrap()]);
    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    process.link("t1", 1).await;
    process.next().await;
    let seen = process.next().await;
    let head = process.state("t1").local_head.unwrap().head_sha;
    assert_eq!(head, git(&checkout, &["rev-parse", "HEAD"]));
    assert!(matches!(&seen, PullRequestEvent::Changed { fields, .. }
        if fields.contains(&PullRequestChangedField::LocalHead)));

    git(&checkout, &["checkout", "--quiet", "-b", "elsewhere"]);
    pass(30).await;

    assert!(
        process.state("t1").local_head.is_none(),
        "the old commit must not stand for it"
    );
    let events = process.quiet_events().await;
    assert!(events.iter().any(|event| matches!(event,
        PullRequestEvent::Changed { fields, .. }
            if fields == &[PullRequestChangedField::LocalHead])));
}

// Given: a fetch under way / When: tracking is switched off, then the answer arrives / Then:
// nothing is stored or published and no git runs for it
#[tokio::test(start_paused = true)]
async fn switching_tracking_off_during_a_fetch_starts_no_git_afterwards() {
    let world = World::new();
    let mut process = world.process(enabled());
    process
        .environment
        .set_directories(vec![world.dir.path().to_path_buf()]);
    let gate = process.source.push_gate(&key(1));
    process.link("t1", 1).await;
    gate.arrived().await;

    process
        .service
        .update_settings(PullRequestsConfig::default())
        .unwrap();
    gate.release_ok(open_running(1));
    let events = process.quiet_events().await;

    assert!(only_changed(&events).is_empty(), "{events:?}");
    assert_eq!(process.environment.directory_requests(), 0);
    let stored = process
        .storage_handle
        .pull_request_snapshot(&key(1))
        .unwrap()
        .and_then(|snapshot| snapshot.facts);
    assert!(stored.is_none());
}

// Given: tracking is switched off while the scheduler reads the registry for its plan / When:
// the plan is done / Then: nothing is started from it
#[tokio::test(start_paused = true)]
async fn switching_tracking_off_while_planning_starts_nothing() {
    let world = World::new();
    let process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.link_directly("t1", 1);
    let service = process.service.clone();
    process.hooks.before_permit(move || {
        service
            .update_settings(PullRequestsConfig::default())
            .unwrap();
    });

    process.host.start("t1").unwrap();
    settle().await;

    assert!(process.source.calls().is_empty());
}

// Given: tracking is switched off while the directories of a checkout are looked up / When: the
// lookup is done / Then: git is not started
#[tokio::test(start_paused = true)]
async fn switching_tracking_off_while_looking_up_directories_starts_no_git() {
    let world = World::new();
    let mut process = world.process(enabled());
    let checkout = world.dir.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    process.environment.set_directories(vec![checkout]);
    let service = process.service.clone();
    process.environment.on_directories(move || {
        service
            .update_settings(PullRequestsConfig::default())
            .unwrap();
    });
    process.source.push_ok(&key(1), open_running(1));

    process.link("t1", 1).await;
    let events = process.quiet_events().await;

    assert_eq!(process.environment.directory_requests(), 1);
    assert_eq!(process.environment.resolutions(), 0);
    assert!(process.state("t1").local_head.is_none(), "{events:?}");
}

// Given: the lease of a thread is handed to another process after the plan found it / When: the
// fetch is about to start / Then: gh is not started for a lease that is gone
#[tokio::test(start_paused = true)]
async fn a_handed_off_lease_starts_no_fetch() {
    let world = World::new();
    let process = world.process(enabled());
    let successor = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.link_directly("t1", 1);
    let (host, next_host) = (Arc::clone(&process.host), Arc::clone(&successor.host));
    process.hooks.after_permit(move |permit| {
        host.handoff(permit, &next_host).unwrap();
    });

    process.host.start("t1").unwrap();
    settle().await;

    assert!(
        process.source.calls().is_empty(),
        "the old owner must not start gh"
    );
}

// Given: the lease of a thread is handed off while the directories of its checkout are looked
// up / When: the lookup is done / Then: git is not started for it
#[tokio::test(start_paused = true)]
async fn a_handed_off_lease_starts_no_git() {
    let world = World::new();
    let mut process = world.process(enabled());
    let successor = world.process(enabled());
    let checkout = world.dir.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    process.environment.set_directories(vec![checkout]);
    let permit = process.permit("t1");
    let (host, next_host) = (Arc::clone(&process.host), Arc::clone(&successor.host));
    process.environment.on_directories(move || {
        host.handoff(&permit, &next_host).unwrap();
    });
    process.source.push_ok(&key(1), open_running(1));

    let owner = process.permit("t1");
    process
        .service
        .link("t1", &format!("{REPOSITORY}#1"), 0, &owner)
        .await
        .unwrap();
    process.quiet_events().await;

    assert_eq!(process.environment.directory_requests(), 1);
    assert_eq!(process.environment.resolutions(), 0);
}

// Given: a pull request waiting for its next poll / When: the settings change at the same time as
// something else wakes the scheduler / Then: the new settings apply whichever wake it reacts to
#[tokio::test(start_paused = true)]
async fn new_settings_apply_to_a_waiting_poll_even_when_another_wake_comes_with_them() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(&key(1), open_passing(1));
    process.source.repeat_last();
    process.link("t1", 1).await;
    process.next().await;
    process.next().await;
    assert_eq!(
        process.source.calls().len(),
        1,
        "the next poll is 300 s away"
    );

    process
        .service
        .update_settings(PullRequestsConfig {
            enabled: true,
            stable_poll_seconds: 60,
            ..PullRequestsConfig::default()
        })
        .unwrap();
    process.service.notify_changed();
    settle().await;

    assert_eq!(
        process.source.calls().len(),
        2,
        "the waiting poll was brought forward"
    );
}

// Given: tracking is off and a link exists / When: time passes / Then: nothing is fetched,
// decisions are refused, and turning it on starts the fetch
#[tokio::test(start_paused = true)]
async fn a_disabled_service_fetches_nothing_and_refuses_decisions() {
    let world = World::new();
    let mut process = world.process(PullRequestsConfig::default());
    process.source.push_ok(&key(1), open_running(1));
    let permit = process.permit("t1");

    let refused = process
        .service
        .link("t1", &format!("{REPOSITORY}#1"), 0, &permit)
        .await;
    process.link_directly("t1", 1);
    pass(3_600).await;

    assert!(matches!(refused, Err(PullRequestError::Disabled)));
    assert!(process.source.calls().is_empty());
    assert!(process.quiet_events().await.is_empty());
    process.service.update_settings(enabled()).unwrap();
    assert!(matches!(
        process.next().await,
        PullRequestEvent::Changed { .. }
    ));
    assert_eq!(process.source.calls().len(), 1);
}

// Given: tracking is on but no thread is linked / When: time passes / Then: nothing is fetched
#[tokio::test(start_paused = true)]
async fn an_unlinked_service_fetches_nothing() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.permit("t1");

    pass(3_600).await;

    assert!(process.source.calls().is_empty());
    assert!(process.quiet_events().await.is_empty());
}

// Given: a link whose checks run, then settle / When: time passes / Then: the fetches come at
// the pending interval, then the stable one, and only a change is published
#[tokio::test(start_paused = true)]
async fn polling_is_quick_while_checks_run_and_slow_once_they_settled() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.source.push_ok(&key(1), open_passing(1));
    process.source.repeat_last();

    process.link("t1", 1).await;
    let linked = process.next().await;
    let baseline = process.next().await;

    assert!(matches!(linked, PullRequestEvent::Linked { .. }));
    let (fields, notifications, first_seq) = changed(&baseline);
    assert_eq!(fields.len(), 6, "the first fetch is a baseline");
    assert!(notifications.is_empty());
    assert_eq!(process.source.calls().len(), 1);

    pass(29).await;
    assert_eq!(process.source.calls().len(), 1, "pending interval is 30 s");
    pass(1).await;
    assert_eq!(process.source.calls().len(), 2);
    let settled = process.next().await;
    let (fields, notifications, second_seq) = changed(&settled);
    assert_eq!(fields, [PullRequestChangedField::Checks]);
    assert_eq!(notifications, ["checks_passed"]);
    assert_eq!(second_seq, first_seq + 1);

    pass(299).await;
    assert_eq!(process.source.calls().len(), 2, "stable interval is 300 s");
    pass(1).await;
    assert_eq!(process.source.calls().len(), 3);
    assert!(
        process.quiet_events().await.is_empty(),
        "a fetch that finds nothing new publishes nothing"
    );
    process.source.push_ok(
        &key(1),
        fetched(1, facts(PullRequestState::Open, failing())),
    );
    process.source.repeat_last();
    pass(300).await;
    let next = process.next().await;
    assert_eq!(
        changed(&next).2,
        second_seq + 1,
        "no transition number was spent on the quiet fetch"
    );
}

// Given: failures of one class, then another, then a success / When: time passes / Then:
// the waits double, the problem is published when its class changes and when it ends, and
// not on every attempt
#[tokio::test(start_paused = true)]
async fn failures_back_off_and_are_published_only_when_their_class_changes() {
    let world = World::new();
    let mut process = world.process(enabled());
    for _ in 0..3 {
        process
            .source
            .push_err(&key(1), FetchError::Network("down".into()));
    }
    process
        .source
        .push_err(&key(1), FetchError::RateLimited("slow down".into()));
    process.source.push_ok(&key(1), open_running(1));
    process.link("t1", 1).await;
    settle().await;
    assert_eq!(process.source.calls().len(), 1);

    pass(59).await;
    assert_eq!(
        process.source.calls().len(),
        1,
        "the first retry waits 60 s"
    );
    pass(1).await;
    assert_eq!(process.source.calls().len(), 2);
    pass(119).await;
    assert_eq!(process.source.calls().len(), 2, "then 120 s");
    pass(1).await;
    assert_eq!(process.source.calls().len(), 3);
    pass(239).await;
    assert_eq!(process.source.calls().len(), 3, "then 240 s");
    pass(1).await;
    assert_eq!(process.source.calls().len(), 4);
    pass(480).await;
    assert_eq!(process.source.calls().len(), 5);

    let events = process.quiet_events().await;
    let statuses: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            PullRequestEvent::FetchStatusChanged { status, .. } => Some(status.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses,
        vec![
            PullRequestFetchStatus::Failing("network".into()),
            PullRequestFetchStatus::Failing("rate_limited".into()),
            PullRequestFetchStatus::Recovered,
        ]
    );
    assert!(
        matches!(events.last(), Some(PullRequestEvent::Changed { .. })),
        "the facts follow the recovery"
    );
}

// Given: a merged pull request / When: time passes / Then: it is not fetched again until a
// refresh finds it open, and then the polling resumes
#[tokio::test(start_paused = true)]
async fn a_merged_pull_request_stops_until_a_refresh_finds_it_open() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(
        &key(1),
        fetched(1, facts(PullRequestState::Merged, passing())),
    );
    let permit = process.permit("t1");
    process.link("t1", 1).await;
    process.next().await;
    process.next().await;

    pass(86_400).await;
    assert_eq!(
        process.source.calls().len(),
        1,
        "a merged pull request is left alone"
    );

    process.source.push_ok(&key(1), open_running(1));
    process.source.repeat_last();
    process.service.refresh("t1", &permit).await.unwrap();
    let reopened = process.next().await;
    let (fields, notifications, _) = changed(&reopened);
    assert!(fields.contains(&PullRequestChangedField::State));
    assert_eq!(notifications, ["reopened"]);
    assert_eq!(process.source.calls().len(), 2);
    pass(30).await;
    assert_eq!(process.source.calls().len(), 3, "polling resumed");
}

// Given: two threads that track one pull request, one fetch held / When: the second links,
// the first unlinks, and the fetch is released / Then: one fetch served both, the unlinked
// thread hears nothing, the other one does, and the facts are stored
#[tokio::test(start_paused = true)]
async fn one_fetch_serves_every_thread_and_an_unlink_does_not_cancel_it() {
    let world = World::new();
    let mut process = world.process(enabled());
    let gate = process.source.push_gate(&key(1));
    let first = process.permit("t1");
    process.link("t1", 1).await;
    gate.arrived().await;

    process.link("t2", 1).await;
    let revision = process.state("t1").lifecycle_revision;
    process
        .service
        .unlink("t1", revision, &first)
        .await
        .unwrap();
    settle().await;
    assert_eq!(process.source.calls().len(), 1, "single-flight");
    gate.release_ok(open_running(1));

    let events = process.quiet_events().await;
    let t1: Vec<_> = events.iter().filter(|e| e.thread_id() == "t1").collect();
    let t2: Vec<_> = events.iter().filter(|e| e.thread_id() == "t2").collect();
    assert!(
        t1.iter()
            .all(|event| !matches!(event, PullRequestEvent::Changed { .. })),
        "{t1:?}"
    );
    assert!(matches!(
        t1.last(),
        Some(PullRequestEvent::Unlinked {
            reason: PullRequestLinkEnd::Unlinked,
            ..
        })
    ));
    assert!(
        t2.iter()
            .any(|event| matches!(event, PullRequestEvent::Changed { .. })),
        "{t2:?}"
    );
    let snapshot = process
        .storage_handle
        .pull_request_snapshot(&key(1))
        .unwrap()
        .unwrap();
    assert!(snapshot.facts.is_some(), "the shared fetch was kept");
    assert_eq!(process.source.calls().len(), 1);
}

// Given: a fetch held for pull request 1 / When: the thread is linked to pull request 2 and
// the old fetch is released / Then: its answer is not delivered to the new link
#[tokio::test(start_paused = true)]
async fn an_answer_for_an_old_link_is_not_delivered_after_a_relink() {
    let world = World::new();
    let mut process = world.process(enabled());
    let gate = process.source.push_gate(&key(1));
    process.source.push_ok(&key(2), open_running(2));
    process.link("t1", 1).await;
    gate.arrived().await;
    process.link("t1", 2).await;
    settle().await;

    gate.release_ok(open_running(1));
    let events = process.quiet_events().await;

    let changes = only_changed(&events);
    assert_eq!(changes.len(), 1, "{events:?}");
    assert_eq!(changes[0].pull_request().number, 2);
    assert!(events.iter().any(|event| matches!(
        event,
        PullRequestEvent::Unlinked {
            reason: PullRequestLinkEnd::Relinked,
            ..
        }
    )));
}

// Given: a decision made from an old window / When: it names an old revision / Then: it is
// refused and the newer link stays
#[tokio::test(start_paused = true)]
async fn a_stale_window_cannot_unlink_a_newer_link() {
    let world = World::new();
    let process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.source.push_ok(&key(2), open_running(2));
    let permit = process.permit("t1");
    process.link("t1", 1).await;
    let stale = process.state("t1").lifecycle_revision;
    process.link("t1", 2).await;

    let outcome = process.service.unlink("t1", stale, &permit).await;

    assert!(matches!(outcome, Err(PullRequestError::StaleRevision)));
    assert_eq!(process.state("t1").active_link.unwrap().key, key(2));
}

// Given: a change that gives reason for a notification / When: it is published / Then: the
// notification key was claimed exactly once and cannot be claimed again
#[tokio::test(start_paused = true)]
async fn a_notification_is_claimed_once_for_its_transition() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.source.push_ok(
        &key(1),
        fetched(1, facts(PullRequestState::Open, failing())),
    );
    process.link("t1", 1).await;
    process.next().await;
    process.next().await;
    pass(30).await;

    let failure = process.next().await;

    let (_, notifications, seq) = changed(&failure);
    assert_eq!(notifications, ["checks_failed"]);
    let link_revision = failure.link_revision();
    let again = process
        .storage_handle
        .claim_pull_request_notification(
            "t1",
            NotificationKey {
                link_revision,
                transition_seq: seq,
                kind: "checks_failed".into(),
            },
        )
        .unwrap();
    assert!(!again, "the service already claimed it");
}

// Given: tracked threads / When: the service runs a whole scenario / Then: it publishes only
// pull request events: no notice, message or wake reaches an agent
#[tokio::test(start_paused = true)]
async fn the_service_never_talks_to_an_agent() {
    let world = World::new();
    let mut process = world.process(enabled());
    process.source.push_ok(&key(1), open_running(1));
    process.source.push_ok(
        &key(1),
        fetched(1, facts(PullRequestState::Open, failing())),
    );
    process.link("t1", 1).await;
    pass(30).await;

    let events = process.quiet_raw_events().await;

    assert!(!events.is_empty());
    assert!(events.iter().all(|event| matches!(
        event.kind,
        EventKind::PullRequest(_) | EventKind::Ownership(_)
    )));
}
