use super::*;
use std::time::UNIX_EPOCH;
use storage::{Database, Storage, StorageConfig};

#[tokio::test]
async fn small_storage_limit_splits_batches_but_rejects_an_oversized_original() {
    // Given: a cap fitting exactly two escaped deltas when measured like storage.
    let chunk = "chunk \"with\" \n🦀 ";
    let delta = |text: String| {
        Event::new(event_bus::MessageEvent::MessageDelta {
            run_id: Some("limited".into()),
            delta: text,
        })
    };
    let base = serde_json::to_vec(&delta(String::new()).kind)
        .unwrap()
        .len();
    let increment = serde_json::to_vec(chunk).unwrap().len() - 2;
    let max_event_bytes = (base + 2 * increment) as u64;
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("small-limit.db"),
        hard_limits: storage::HardLimits {
            max_event_bytes,
            ..Default::default()
        },
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let bus = Arc::new(EventBus::new(64));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let monitor = bridge.monitor();
    let task = tokio::spawn(run(bus.clone(), bridge, Duration::from_secs(60)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while bus.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    // When: valid originals arrive in a burst, followed by one oversized original.
    for _ in 0..25 {
        bus.emit(delta(chunk.into()));
    }
    bus.emit(delta(chunk.repeat(3)));
    drop(bus);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();

    // Then: batching never invalidates a valid original or bypasses the hard cap.
    let stored = db.events_all_ordered().unwrap();
    assert_eq!(stored.len(), 13);
    let mut actual = String::new();
    for row in stored {
        assert!(serde_json::to_vec(&row.event.kind).unwrap().len() as u64 <= max_event_bytes);
        let EventKind::Message(event_bus::MessageEvent::MessageDelta { run_id, delta }) =
            row.event.kind
        else {
            panic!("expected delta")
        };
        assert_eq!(run_id.as_deref(), Some("limited"));
        actual.push_str(&delta);
    }
    assert_eq!(actual, chunk.repeat(25));
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.persisted_events, 13);
    assert_eq!(snapshot.coalesced_events, 12);
    assert_eq!(snapshot.failed_events, 1);
    assert_eq!(snapshot.pending_events, 0);
}

#[tokio::test]
async fn midnight_deltas_keep_separate_rows_and_independent_daily_budgets() {
    // Given: adjacent deltas 20 ms apart, on opposite sides of UTC midnight.
    let mut before = Event::new(event_bus::MessageEvent::MessageDelta {
        run_id: Some("midnight".into()),
        delta: "before".into(),
    });
    before.meta.wall_clock = UNIX_EPOCH + Duration::from_millis(86_400_000 - 10);
    let mut after = before.clone();
    after.meta.wall_clock = UNIX_EPOCH + Duration::from_millis(86_400_000 + 10);
    let EventKind::Message(event_bus::MessageEvent::MessageDelta { delta, .. }) = &mut after.kind
    else {
        unreachable!()
    };
    *delta = "after!".into();
    let max_daily_event_bytes = serde_json::to_vec(&before.kind).unwrap().len() as u64;
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("midnight.db"),
        hard_limits: storage::HardLimits {
            max_daily_event_bytes,
            ..Default::default()
        },
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let bus = Arc::new(EventBus::new(32));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let monitor = bridge.monitor();
    let task = tokio::spawn(run(bus.clone(), bridge, Duration::from_secs(60)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while bus.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    // When: each day accepts its full budget before the bridge shuts down.
    bus.emit(before.clone());
    bus.emit(after.clone());
    drop(bus);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();

    // Then: neither day's bytes are attributed to the other; timestamps survive.
    let stored: Vec<_> = db
        .events_all_ordered()
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect();
    assert_eq!(stored, [before, after]);
    assert_eq!(monitor.snapshot().failed_events, 0);
    assert_eq!(monitor.snapshot().coalesced_events, 0);
}

#[tokio::test]
async fn invalid_timestamp_deltas_remain_independently_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("timestamps.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let bus = Arc::new(EventBus::new(32));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let monitor = bridge.monitor();
    let task = tokio::spawn(run(bus.clone(), bridge, Duration::from_secs(60)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while bus.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut valid = Event::new(event_bus::MessageEvent::MessageDelta {
        run_id: Some("run".into()),
        delta: "accepted".into(),
    });
    valid.meta.wall_clock = UNIX_EPOCH + Duration::from_nanos(i64::MAX as u64 - 1);
    let mut overflow = valid.clone();
    overflow.meta.wall_clock = UNIX_EPOCH + Duration::from_nanos(i64::MAX as u64 + 1);
    let mut pre_epoch = valid.clone();
    pre_epoch.meta.wall_clock = UNIX_EPOCH - Duration::from_nanos(1);
    for event in [valid.clone(), overflow, pre_epoch] {
        bus.emit(event);
    }
    drop(bus);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    let stored: Vec<_> = db
        .events_all_ordered()
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect();
    assert_eq!(stored, [valid]);
    assert_eq!(monitor.snapshot().failed_events, 2);
    assert_eq!(monitor.snapshot().coalesced_events, 0);
    assert_eq!(monitor.snapshot().pending_bytes, 0);
}
