use std::error::Error;
use std::time::{Duration, UNIX_EPOCH};

use event_bus::{Event, EventMeta, LifecycleEvent, UsageAggregator, UsageEvent, UsageSink};
use rusqlite::Connection;
use storage::projection::restore_sessions;
use storage::repo::metrics::list_range;
use storage::{Storage, StorageConfig};

fn main() -> Result<(), Box<dyn Error>> {
    // Given: 一時 DB 上の single-writer と同一分に属する usage イベント
    let temp_dir = tempfile::tempdir()?;
    let db_path = temp_dir.path().join("storage-demo.db");
    let storage = Storage::open(StorageConfig {
        db_path: db_path.clone(),
        ..StorageConfig::default()
    })?;
    let handle = storage.handle();
    let meta = EventMeta {
        schema_version: event_bus::SCHEMA_VERSION,
        monotonic: Duration::ZERO,
        wall_clock: UNIX_EPOCH + Duration::from_secs(120),
    };
    let mut aggregator = UsageAggregator::new();
    for usage in [
        UsageEvent::Usage {
            provider: "anthropic".into(),
            model: "claude-sonnet".into(),
            input_tokens: 120,
            output_tokens: 30,
            cache_read_tokens: 80,
            cache_write_tokens: 10,
        },
        UsageEvent::Usage {
            provider: "anthropic".into(),
            model: "claude-sonnet".into(),
            input_tokens: 40,
            output_tokens: 12,
            cache_read_tokens: 20,
            cache_write_tokens: 5,
        },
        UsageEvent::CacheStats {
            provider: "anthropic".into(),
            model: "claude-sonnet".into(),
            cache_hits: 9,
            cache_misses: 2,
        },
    ] {
        aggregator.record(&usage, &meta);
    }

    // When: UsageSink と event writer へ投入し、flush 後に clean shutdown する
    let sink: &dyn UsageSink = &handle;
    aggregator.flush_into(sink);
    handle.flush_usage_now()?;
    handle.append_event(
        Some("session-demo-001"),
        &Event {
            meta,
            kind: LifecycleEvent::Started {
                session_id: "session-demo-001".into(),
            }
            .into(),
        },
    )?;
    storage.close();

    // Then: 第二接続から metrics と event projection を復元して表示する
    let connection = Connection::open(&db_path)?;
    let metrics = list_range(&connection, 0, i64::MAX as u64)?;
    let sessions = restore_sessions(&connection)?;
    println!("metrics: {metrics:#?}");
    println!("sessions: {sessions:#?}");
    println!("database: {}", db_path.display());
    Ok(())
}
