//! raw usage event の永続化拒否境界を検証します。

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{BucketKey, Event, EventMeta, LifecycleEvent, UsageBucket, UsageEvent, UsageSink};
use storage::{Database, Storage, StorageConfig, StorageError};
use tempfile::TempDir;

fn config(temp_dir: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: temp_dir.path().join("evorch.db"),
        ..StorageConfig::default()
    }
}

fn event(kind: impl Into<event_bus::EventKind>, nanos: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_nanos(nanos),
            wall_clock: UNIX_EPOCH + Duration::from_nanos(nanos),
        },
        kind: kind.into(),
    }
}

fn raw_usage_event(nanos: u64) -> Event {
    event(
        UsageEvent::Usage {
            provider: "provider".into(),
            model: "model".into(),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_write_tokens: 40,
        },
        nanos,
    )
}

#[test]
fn storage_handle_rejects_raw_usage_before_insert_and_preserves_non_usage_events() {
    // Given: 開いた Storage、raw usage event、通常の lifecycle event
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let usage = raw_usage_event(1);
    let lifecycle = event(
        LifecycleEvent::Started {
            session_id: "session-1".into(),
        },
        2,
    );

    // When: 公開 handle へ raw usage を渡し、通常 event は追記する
    let error = handle
        .append_event(None, &usage)
        .expect_err("raw usage event must be rejected");
    handle
        .append_event(None, &lifecycle)
        .expect("non-usage event must append");
    storage.close();

    // Then: actionable error を返し、events には通常 event だけが残る
    assert_eq!(error, StorageError::RawUsageEventNotPersisted);
    let message = error.to_string();
    assert!(message.contains("raw usage events are not persisted"));
    assert!(message.contains("UsageSink"));
    let database = Database::open(&config).expect("database must reopen");
    let stored = database.events_all_ordered().expect("events must list");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].event, lifecycle);
    println!("raw_usage_error={message}");
    println!("non_usage_event_count={}", stored.len());
}

#[test]
fn usage_sink_still_persists_downsampled_metrics() {
    // Given: 開いた Storage と downsampled usage bucket
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let expected = UsageBucket {
        key: BucketKey {
            window_start: 60,
            provider: "provider".into(),
            model: "model".into(),
        },
        input_tokens: 10,
        output_tokens: 20,
        cache_read_tokens: 30,
        cache_write_tokens: 40,
        cache_hits: 5,
        cache_misses: 6,
        request_count: 1,
    };

    // When: 正規の UsageSink 経路へ投入して flush する
    let sink: &dyn UsageSink = &handle;
    sink.submit(vec![expected.clone()]);
    handle.flush_usage_now().expect("usage must flush");
    storage.close();

    // Then: downsampled_metrics に一行保存される
    let database = Database::open(&config).expect("database must reopen");
    let stored = database
        .metrics_range(0, i64::MAX as u64)
        .expect("metrics must list");
    assert_eq!(stored, [expected]);
    println!("downsampled_metrics_count={}", stored.len());
}
