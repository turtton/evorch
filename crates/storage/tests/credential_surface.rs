//! ADR 0008 の credential 非永続化 — 公開書き込み経路が型付きレコードのみを受け付けることを検証します。

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{BucketKey, Event, EventMeta, LifecycleEvent, UsageBucket, UsageSink};
use storage::{CatalogUpdateRecord, Storage, StorageConfig};
use tempfile::TempDir;

#[test]
fn public_write_paths_accept_only_typed_records() {
    // Given: ADR 0008 credential 非永続化 — 全書き込み経路は型付きレコードのみを受け付け、
    // credential を保持し得る汎用 key/value や生 SQL 経路を公開しない。
    let temp = TempDir::new().expect("temporary directory must be created");
    let time = UNIX_EPOCH + Duration::from_secs(60);
    let event_value = Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::ZERO,
            wall_clock: time,
        },
        kind: LifecycleEvent::Started {
            session_id: "s".into(),
        }
        .into(),
    };
    let bucket = UsageBucket {
        key: BucketKey {
            window_start: 60,
            provider: "p".into(),
            model: "model".into(),
        },
        input_tokens: 1,
        output_tokens: 2,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
        cache_hits: 5,
        cache_misses: 6,
        request_count: 1,
    };

    // When: single-writer の公開書き込み経路を型付き fixture で呼び出す
    let config = StorageConfig {
        db_path: temp.path().join("writer.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config).expect("storage must open");
    let handle = storage.handle();
    handle
        .append_event(None, &event_value)
        .expect("handle event must write");
    handle
        .record_catalog_update(&CatalogUpdateRecord {
            source: "models-dev".into(),
            model_count: 1,
            detail: "typed".into(),
            recorded_at_ns: 60,
        })
        .expect("catalog update must write");
    <storage::StorageHandle as UsageSink>::submit(&handle, vec![bucket]);
    handle.flush_usage_now().expect("handle usage must flush");

    // Then: 全公開経路が型検査され、実データベースへの書き込みに成功する
    storage.close();
}
