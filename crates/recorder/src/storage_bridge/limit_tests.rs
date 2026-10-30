use super::*;
use std::time::UNIX_EPOCH;
use storage::{Database, Storage, StorageConfig};

#[test]
fn synchronous_bridge_also_preserves_oversized_completed_text() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("sync-parts.db"),
        hard_limits: storage::HardLimits {
            max_event_bytes: 200,
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let event = Event::new(event_bus::MessageEvent::MessageCompleted {
        run_id: "r".into(),
        text: "あ\"\\\n".repeat(300),
    });
    bridge.handle_event(&event).unwrap();
    storage.close();
    let db = Database::open(&config).unwrap();
    let mut assembler = event_bus::MessagePartAssembler::new();
    let mut completions = Vec::new();
    let rows = db.events_all_ordered().unwrap();
    assert!(rows.len() > 1);
    for row in &rows {
        assert!(serde_json::to_vec(&row.event.kind).unwrap().len() <= 200);
        if let Some(done) = assembler.accept(&row.event) {
            completions.push(done.into_owned());
        }
    }
    assert_eq!(completions, [event]);
}

#[tokio::test]
async fn oversized_single_unicode_and_escaped_fragments_respect_every_row_cap() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("single-fragment.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let bus = Arc::new(EventBus::new(32));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let monitor = bridge.monitor();
    let task = spawn_test_bridge(&bus, bridge, Duration::from_secs(3600));
    let expected = ["あ".repeat(30_000), "\"\\\n\u{1}🦀".repeat(12_000)];
    for (run, text) in [Some("a"), None].into_iter().zip(&expected) {
        bus.emit(Event::new(event_bus::MessageEvent::ReasoningDelta {
            run_id: run.map(str::to_owned),
            delta: text.clone(),
        }));
    }
    drop(bus);
    task.await.unwrap();
    let mut actual = [String::new(), String::new()];
    let mut counts = [0, 0];
    for row in db.events_all_ordered().unwrap() {
        assert!(serde_json::to_vec(&row.event.kind).unwrap().len() <= 64 * 1024);
        let EventKind::Message(event_bus::MessageEvent::ReasoningDelta { run_id, delta }) =
            row.event.kind
        else {
            panic!("expected reasoning")
        };
        let index = usize::from(run_id.is_none());
        actual[index].push_str(&delta);
        counts[index] += 1;
    }
    assert_eq!(actual, expected);
    assert!(counts.into_iter().all(|count| count > 1));
    assert_eq!(monitor.snapshot().failed_events, 0);
}

#[tokio::test]
async fn oversized_secret_text_is_rejected_whole_instead_of_hidden_by_splitting() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("guard.db"),
        hard_limits: storage::HardLimits {
            max_event_bytes: 200,
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let bus = Arc::new(EventBus::new(32));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let monitor = bridge.monitor();
    let task = spawn_test_bridge(&bus, bridge, Duration::from_secs(3600));
    // Construct a synthetic shape without logging its value in diagnostics.
    let fixture = ["sk", "-", &"a".repeat(80)].concat();
    assert!(
        secret_guard::SecretRedactor::from_env()
            .detect(&fixture)
            .is_some()
    );
    let text = format!("{}{}{}", " ".repeat(90), fixture, " ".repeat(100));
    bus.emit(Event::new(event_bus::MessageEvent::ReasoningDelta {
        run_id: Some("r".into()),
        delta: text.clone(),
    }));
    bus.emit(Event::new(event_bus::MessageEvent::MessageCompleted {
        run_id: "r".into(),
        text,
    }));
    drop(bus);
    task.await.unwrap();
    assert!(db.events_all_ordered().unwrap().is_empty());
    assert_eq!(monitor.snapshot().failed_events, 2);
    assert_eq!(monitor.snapshot().persisted_events, 0);
}

#[tokio::test]
async fn small_storage_limit_splits_batches_and_oversized_originals_without_loss() {
    // Given: a cap fitting exactly two escaped deltas when measured like storage.
    let chunk = "chunk \"with\" \n🦀 ";
    let delta = |text: String| {
        Event::new(event_bus::MessageEvent::ReasoningDelta {
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
    let task = spawn_test_bridge(&bus, bridge, Duration::from_secs(60));

    // When: valid originals arrive in a burst, followed by one original above the cap.
    for _ in 0..25 {
        bus.emit(delta(chunk.into()));
    }
    bus.emit(delta(chunk.repeat(3)));
    drop(bus);
    task.await.unwrap();

    // Then: every row respects the hard cap and no text is lost, including the
    // oversized original, which is split instead of rejected.
    let stored = db.events_all_ordered().unwrap();
    assert_eq!(stored.len(), 15);
    let mut actual = String::new();
    for row in stored {
        assert!(serde_json::to_vec(&row.event.kind).unwrap().len() as u64 <= max_event_bytes);
        let EventKind::Message(event_bus::MessageEvent::ReasoningDelta { run_id, delta }) =
            row.event.kind
        else {
            panic!("expected delta")
        };
        assert_eq!(run_id.as_deref(), Some("limited"));
        actual.push_str(&delta);
    }
    assert_eq!(actual, chunk.repeat(28));
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.persisted_events, 15);
    assert_eq!(snapshot.coalesced_events, 12);
    assert_eq!(snapshot.failed_events, 0);
    assert_eq!(snapshot.pending_events, 0);
}

#[tokio::test]
async fn midnight_deltas_keep_separate_rows_and_independent_daily_budgets() {
    // Given: adjacent deltas 20 ms apart, on opposite sides of UTC midnight.
    let mut before = Event::new(event_bus::MessageEvent::ReasoningDelta {
        run_id: Some("midnight".into()),
        delta: "before".into(),
    });
    before.meta.wall_clock = UNIX_EPOCH + Duration::from_millis(86_400_000 - 10);
    let mut after = before.clone();
    after.meta.wall_clock = UNIX_EPOCH + Duration::from_millis(86_400_000 + 10);
    let EventKind::Message(event_bus::MessageEvent::ReasoningDelta { delta, .. }) = &mut after.kind
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
    let task = spawn_test_bridge(&bus, bridge, Duration::from_secs(60));

    // When: each day accepts its full budget before the bridge shuts down.
    bus.emit(before.clone());
    bus.emit(after.clone());
    drop(bus);
    task.await.unwrap();

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
    let task = spawn_test_bridge(&bus, bridge, Duration::from_secs(60));

    let mut valid = Event::new(event_bus::MessageEvent::ReasoningDelta {
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
    task.await.unwrap();
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
