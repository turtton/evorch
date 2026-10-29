//! The follower reads the shared event log only when it changed, in order,
//! without its own rows, and without losing or repeating any.

use event_bus::{Event, EventKind, LifecycleEvent};
use gui::shared_state::{
    FedEvent, FeedBatch, FeedConfig, FeedPoller, FeedWorker, PAGE_SIZE, POLL_ROW_LIMIT,
};
use storage::{Storage, StorageConfig};
use tempfile::TempDir;

fn started(label: impl Into<String>) -> Event {
    Event::new(LifecycleEvent::Started {
        session_id: label.into(),
    })
}

fn labels(events: &[FedEvent]) -> Vec<String> {
    events
        .iter()
        .map(|fed| match &fed.event.kind {
            EventKind::Lifecycle(LifecycleEvent::Started { session_id }) => session_id.clone(),
            other => panic!("unexpected event {other:?}"),
        })
        .collect()
}

fn setup(name: &str) -> (TempDir, StorageConfig, Storage) {
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join(name),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    (dir, config, storage)
}

fn poller(config: &StorageConfig, own: &str, cursor: i64) -> FeedPoller {
    FeedPoller::open(FeedConfig {
        db_path: config.db_path.clone(),
        own_stream: own.into(),
        cursor,
        sidebar_path: None,
    })
    .expect("poller must open")
}

#[test]
fn six_hundred_events_arrive_in_order_in_pages_and_nothing_is_read_while_idle() {
    // Given: 600 events from another process
    let (_dir, config, storage) = setup("six-hundred.db");
    let handle = storage.handle();
    for index in 0..600 {
        handle
            .append_stream_event("other", &started(format!("e{index:03}")), None)
            .expect("append");
    }
    let mut follower = poller(&config, "me", 0);

    // When: the follower polls
    let batch = follower.poll_once().expect("poll");

    // Then: every event arrived once, in order, in 256-row queries (256 + 256 + 88)
    let expected: Vec<String> = (0..600).map(|i| format!("e{i:03}")).collect();
    assert_eq!(labels(&batch.events), expected);
    assert_eq!(follower.event_queries(), 3);
    assert_eq!(follower.cursor(), 600);
    let ids: Vec<i64> = batch.events.iter().map(|fed| fed.id).collect();
    assert_eq!(ids, (1..=600).collect::<Vec<i64>>());
    assert!(!batch.backlog);

    // And: polls while nothing was committed run no event query
    for _ in 0..10 {
        assert!(follower.poll_once().expect("poll").events.is_empty());
    }
    assert_eq!(follower.event_queries(), 3, "idle polls must not query");

    // And: a new commit is picked up with exactly one more query
    handle
        .append_stream_event("other", &started("late"), None)
        .expect("append");
    let batch = follower.poll_once().expect("poll");
    assert_eq!(labels(&batch.events), ["late"]);
    assert_eq!(follower.event_queries(), 4);
}

#[test]
fn own_stream_rows_are_skipped_but_still_advance_the_cursor() {
    // Given: this process's rows interleaved with another's
    let (_dir, config, storage) = setup("own.db");
    let handle = storage.handle();
    for (stream, label) in [("me", "m0"), ("other", "o1"), ("me", "m2"), ("me", "m3")] {
        handle
            .append_stream_event(stream, &started(label), None)
            .expect("append");
    }
    let mut follower = poller(&config, "me", 0);

    // When: it polls
    let batch = follower.poll_once().expect("poll");

    // Then: only the other stream's event arrives, and the trailing own rows are not rescanned
    assert_eq!(labels(&batch.events), ["o1"]);
    assert_eq!(follower.cursor(), 4);
    handle
        .append_stream_event("other", &started("o4"), None)
        .expect("append");
    assert_eq!(labels(&follower.poll_once().expect("poll").events), ["o4"]);
}

#[test]
fn starting_after_the_replayed_cursor_does_not_repeat_history() {
    // Given: 5 events already reflected by a restart's replay (cursor 5)
    let (_dir, config, storage) = setup("cursor.db");
    let handle = storage.handle();
    for index in 0..8 {
        handle
            .append_stream_event("other", &started(format!("e{index}")), None)
            .expect("append");
    }

    // When: following starts at the replay's end
    let mut follower = poller(&config, "me", 5);

    // Then: only the later events arrive
    assert_eq!(
        labels(&follower.poll_once().expect("poll").events),
        ["e5", "e6", "e7"]
    );
}

#[test]
fn a_backlog_beyond_one_polls_budget_continues_without_loss() {
    // Given: more rows than one poll may scan
    let (_dir, config, storage) = setup("backlog.db");
    let handle = storage.handle();
    let total = POLL_ROW_LIMIT + PAGE_SIZE + 5;
    for index in 0..total {
        handle
            .append_stream_event("other", &started(format!("e{index:05}")), None)
            .expect("append");
    }
    let mut follower = poller(&config, "me", 0);

    // When: polling until the backlog is gone
    let first = follower.poll_once().expect("poll");
    assert!(first.backlog);
    assert_eq!(first.events.len(), POLL_ROW_LIMIT);
    let second = follower.poll_once().expect("poll");

    // Then: the remainder follows, in order, with nothing repeated
    assert!(!second.backlog);
    let mut all = labels(&first.events);
    all.extend(labels(&second.events));
    let expected: Vec<String> = (0..total).map(|i| format!("e{i:05}")).collect();
    assert_eq!(all, expected);
}

#[test]
fn the_worker_queues_batches_signals_a_repaint_and_stops_on_drop() {
    // Given: a worker following the log, with a repaint hook that signals a channel
    let (_dir, config, storage) = setup("worker.db");
    let (signal, signalled) = std::sync::mpsc::channel();
    let repaint: std::sync::Arc<dyn Fn() + Send + Sync> = {
        let signal = std::sync::Mutex::new(signal);
        std::sync::Arc::new(move || {
            let _ = signal.lock().expect("signal").send(());
        })
    };
    let worker = FeedWorker::spawn(poller(&config, "me", 0), Some(repaint)).expect("worker");

    // When: another process persists an event
    storage
        .handle()
        .append_stream_event("other", &started("from-elsewhere"), None)
        .expect("append");

    // Then: the worker queues it and asks for a repaint (awaited on the signal, not on a timer)
    let mut seen = Vec::new();
    while seen.is_empty() {
        signalled.recv().expect("repaint signal");
        while let Some(batch) = worker.try_next() {
            seen.extend(labels(&batch.events));
        }
    }
    assert_eq!(seen, ["from-elsewhere"]);

    // And: dropping the worker stops its thread
    drop(worker);
}

#[test]
fn dropping_a_worker_whose_queue_is_full_returns() {
    // Given: a worker that always has another batch, with a GUI that never takes any
    let (signal, signalled) = std::sync::mpsc::channel();
    let repaint: std::sync::Arc<dyn Fn() + Send + Sync> = {
        let signal = std::sync::Mutex::new(signal);
        std::sync::Arc::new(move || {
            let _ = signal.lock().expect("signal").send(());
        })
    };
    let worker = FeedWorker::spawn_source(
        || {
            Ok(FeedBatch {
                events: vec![FedEvent {
                    id: 1,
                    event: started("again"),
                }],
                backlog: true,
                ..FeedBatch::default()
            })
        },
        Some(repaint),
    )
    .expect("worker");

    // When: the queue (16 batches) is full and the worker is blocked sending the next
    for _ in 0..16 {
        signalled.recv().expect("repaint signal");
    }

    // Then: dropping the worker still stops it (a deadlock is caught by the runner's watchdog)
    drop(worker);
}
