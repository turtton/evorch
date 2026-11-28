//! The pull request service on its production paths: storage and git run
//! on the blocking pool of a multi-threaded runtime, with a real clock. (The unit tests run that
//! work inline so that a paused clock cannot move under it; this file covers what they cannot.)
//! Every wait is for an event or a gate, never for a duration.

use std::sync::Arc;

use config::PullRequestsConfig;
use event_bus::{EventBus, EventKind, PullRequestEvent};
use runtime::pull_requests::{
    FakePullRequestSource, FetchedPullRequest, PullRequestService, ServiceParts, SystemWallClock,
    ThreadEnvironment,
};
use storage::pull_requests::{PullRequestFacts, PullRequestKey, PullRequestState, ReviewDecision};

struct Nothing;

impl ThreadEnvironment for Nothing {
    fn thread_of_run(&self, _run_id: &str) -> Option<String> {
        None
    }

    fn checkout_directories(&self, _thread_id: &str) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

fn key() -> PullRequestKey {
    PullRequestKey::github("turtton/evorch", 1).unwrap()
}

fn answer() -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(),
        canonical: key(),
        url: "https://github.com/turtton/evorch/pull/1".into(),
        display: Default::default(),
        facts: PullRequestFacts {
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_repository: Some("turtton/evorch".into()),
            head_branch: "feat".into(),
            head_sha: "a".repeat(40),
            checks: Vec::new(),
            checks_complete: true,
            review_decision: ReviewDecision::None,
        },
        fetched_at_ns: 1,
    }
}

struct Setup {
    _directory: test_socket_dir::SplitDir,
    _storage: storage::Storage,
    source: FakePullRequestSource,
    service: PullRequestService,
    events: event_bus::EventReceiver,
}

fn setup(enabled: bool) -> Setup {
    let directory = test_socket_dir::SplitDir::new();
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: directory.path().join("service.db"),
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let events = bus.subscribe();
    let source = FakePullRequestSource::new();
    let service = PullRequestService::start(ServiceParts {
        storage: storage.handle(),
        source: Arc::new(source.clone()),
        environment: Arc::new(Nothing),
        bus,
        settings: PullRequestsConfig {
            enabled,
            ..Default::default()
        },
        clock: Arc::new(SystemWallClock),
    })
    .unwrap();
    Setup {
        _directory: directory,
        _storage: storage,
        source,
        service,
        events,
    }
}

async fn next(setup: &mut Setup) -> PullRequestEvent {
    loop {
        if let EventKind::PullRequest(event) = setup.events.recv().await.unwrap().kind {
            return event;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_is_fetched_and_published_through_the_blocking_pool() {
    // Given: a thread.
    let mut setup = setup(true);
    setup.source.push_ok(&key(), answer());

    // When: it is linked.
    setup
        .service
        .link("thread-1", "turtton/evorch#1", 0)
        .await
        .unwrap();

    // Then: the link and the first facts are published, and the facts are stored.
    assert!(matches!(
        next(&mut setup).await,
        PullRequestEvent::Linked { .. }
    ));
    assert!(matches!(
        next(&mut setup).await,
        PullRequestEvent::Changed { .. }
    ));
    let snapshot = setup
        ._storage
        .handle()
        .pull_request_snapshot(&key())
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.facts.unwrap().state, PullRequestState::Open);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unlink_during_a_fetch_delivers_nothing_to_that_thread() {
    // Given: three threads linked to one pull request, all prepared while tracking is
    // off, so that the first plan sees all of them and one fetch is started for the three.
    let mut setup = setup(false);
    let gate = setup.source.push_gate(&key());
    for thread in ["thread-1", "thread-2", "thread-3"] {
        setup
            ._storage
            .handle()
            .link_pull_request(storage::pull_requests::LinkPullRequest {
                thread_id: thread.into(),
                key: key(),
                expected_revision: 0,
                at_ns: 1,
            })
            .unwrap();
    }
    setup
        .service
        .update_settings(PullRequestsConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
    gate.arrived().await;

    // When: the first thread unlinks and the fetch is released.
    setup.service.unlink("thread-1", 1).await.unwrap();
    gate.release_ok(answer());

    // Then: only the threads that still have their link hear the change. All three took part
    // in the one fetch, and deliveries go in link order, so the third one's change closes the
    // other two.
    let mut changed = Vec::new();
    loop {
        if let PullRequestEvent::Changed { thread_id, .. } = next(&mut setup).await {
            changed.push(thread_id.clone());
            if thread_id == "thread-3" {
                break;
            }
        }
    }
    assert_eq!(changed, vec!["thread-2".to_owned(), "thread-3".to_owned()]);
    assert_eq!(setup.source.calls().len(), 1, "one fetch served the three");
}
