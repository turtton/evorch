//! Health observations must not become another persistent metrics sink.
use event_bus::{BucketKey, Event, MessageEvent, UsageBucket, UsageSink};
use storage::{Storage, StorageConfig, StorageStatistics};

#[test]
fn statistics_and_empty_usage_flush_do_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("statistics.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let wal = config.db_path.with_extension("db-wal");
    let initial_len = std::fs::metadata(&wal).unwrap().len();
    for _ in 0..64 {
        assert_eq!(handle.statistics().unwrap(), StorageStatistics::default());
        handle.flush_usage_now().unwrap();
    }
    assert_eq!(std::fs::metadata(wal).unwrap().len(), initial_len);
}

#[test]
fn statistics_count_committed_payloads_and_nonempty_usage_only() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("statistics.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config).unwrap();
    let handle = storage.handle();
    let event = Event::new(MessageEvent::MessageDelta {
        delta: "日本語".into(),
        run_id: None,
    });
    handle.append_stream_event("stream", &event, None).unwrap();
    let too_large = Event::new(MessageEvent::MessageDelta {
        delta: "x".repeat(handle.max_event_bytes() as usize),
        run_id: None,
    });
    assert!(
        handle
            .append_stream_event("stream", &too_large, None)
            .is_err()
    );
    handle.submit(vec![UsageBucket {
        key: BucketKey {
            window_start: 60,
            provider: "provider".into(),
            model: "model".into(),
        },
        input_tokens: 2,
        output_tokens: 3,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_hits: 0,
        cache_misses: 1,
        request_count: 1,
    }]);
    handle.flush_usage_now().unwrap();
    handle.flush_usage_now().unwrap();
    let stats = handle.statistics().unwrap();
    assert_eq!(stats.events_written, 1);
    assert_eq!(
        stats.event_payload_bytes,
        serde_json::to_vec(&event.kind).unwrap().len() as u64
    );
    assert_eq!(stats.event_write_failures, 1);
    assert_eq!(stats.usage_flushes, 1);
    assert_eq!(stats.usage_buckets_written, 1);
    assert!(stats.max_append_micros <= stats.append_total_micros);
}

#[test]
fn statistics_include_fence_and_suspended_write_rejections() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("fenced.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config).unwrap();
    let bus = event_bus::EventBus::new(8);
    bus.register_mutation_fence("stale".into(), std::sync::Arc::new(|| false));
    let event = Event::new(MessageEvent::MessageDelta {
        delta: "text".into(),
        run_id: Some("stale".into()),
    });
    assert!(
        storage
            .handle()
            .append_stream_event("stream", &event, Some(bus.mutation_validator()))
            .is_err()
    );
    assert_eq!(
        storage.handle().statistics().unwrap().event_write_failures,
        1
    );
    let mut config = StorageConfig {
        db_path: dir.path().join("suspended.db"),
        ..StorageConfig::default()
    };
    config.hard_limits.max_db_bytes = 0;
    let suspended = Storage::open(config).unwrap();
    assert!(suspended.handle().append_event(None, &event).is_err());
    assert_eq!(
        suspended
            .handle()
            .statistics()
            .unwrap()
            .event_write_failures,
        1
    );
}
