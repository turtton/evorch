//! The pull request service on its production paths: storage, the ownership registry and git run
//! on the blocking pool of a multi-threaded runtime, with a real clock. (The unit tests run that
//! work inline so that a paused clock cannot move under it; this file covers what they cannot.)
//! Every wait is for an event or a gate, never for a duration.

use std::sync::Arc;

use config::PullRequestsConfig;
use event_bus::{EventBus, EventKind, PullRequestEvent};
use runtime::ownership::OwnerHost;
use runtime::pull_requests::{
    FakePullRequestSource, FetchedPullRequest, OwnershipView, PullRequestService, ServiceParts,
    SystemWallClock, ThreadEnvironment,
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
    _directory: tempfile::TempDir,
    _storage: storage::Storage,
    host: Arc<OwnerHost>,
    source: FakePullRequestSource,
    service: PullRequestService,
    events: event_bus::EventReceiver,
}

fn setup() -> Setup {
    let directory = tempfile::tempdir().unwrap();
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: directory.path().join("service.db"),
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let events = bus.subscribe();
    let host = Arc::new(
        OwnerHost::open(
            &directory.path().join("owners"),
            config::OwnershipConfig::default(),
            Arc::clone(&bus),
        )
        .unwrap(),
    );
    let source = FakePullRequestSource::new();
    let service = PullRequestService::start(ServiceParts {
        storage: storage.handle(),
        source: Arc::new(source.clone()),
        ownership: Arc::clone(&host) as Arc<dyn OwnershipView>,
        environment: Arc::new(Nothing),
        bus,
        settings: PullRequestsConfig {
            enabled: true,
            ..Default::default()
        },
        clock: Arc::new(SystemWallClock),
    })
    .unwrap();
    Setup {
        _directory: directory,
        _storage: storage,
        host,
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
    // Given: an owned thread.
    let mut setup = setup();
    setup.source.push_ok(&key(), answer());
    let permit = setup.host.start("thread-1").unwrap();

    // When: it is linked.
    setup
        .service
        .link("thread-1", "turtton/evorch#1", 0, &permit)
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
async fn an_unlink_and_a_lost_lease_during_a_fetch_deliver_nothing_to_that_thread() {
    // Given: two threads on one pull request, the fetch held, the first thread unlinked and the
    // second one's lease moved to another host.
    let mut setup = setup();
    let gate = setup.source.push_gate(&key());
    let (first, second) = (
        setup.host.start("thread-1").unwrap(),
        setup.host.start("thread-2").unwrap(),
    );
    let third = setup.host.start("thread-3").unwrap();
    for (thread, permit) in [
        ("thread-1", &first),
        ("thread-2", &second),
        ("thread-3", &third),
    ] {
        setup
            .service
            .link(thread, "turtton/evorch#1", 0, permit)
            .await
            .unwrap();
    }
    gate.arrived().await;
    setup.service.unlink("thread-1", 1, &first).await.unwrap();
    let successor = Arc::new(
        OwnerHost::open(
            &setup._directory.path().join("owners"),
            config::OwnershipConfig::default(),
            Arc::new(EventBus::new(16)),
        )
        .unwrap(),
    );
    setup.host.handoff(&second, &successor).unwrap();

    // When: the fetch is released.
    gate.release_ok(answer());

    // Then: only the thread that still has its link and its lease hears the change. Deliveries
    // go in the order the threads joined, so the third one's change closes the other two.
    let mut changed = Vec::new();
    loop {
        if let PullRequestEvent::Changed { thread_id, .. } = next(&mut setup).await {
            changed.push(thread_id.clone());
            if thread_id == "thread-3" {
                break;
            }
        }
    }
    assert_eq!(changed, vec!["thread-3".to_owned()]);
}
