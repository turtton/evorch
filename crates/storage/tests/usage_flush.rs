//! single-writer の usage 永続化と容量制御を検証します。

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{
    BucketKey, Event, EventMeta, LifecycleEvent, MessageEvent, UsageBucket, UsageSink,
};
use rusqlite::Connection;
use storage::repo::metrics::list_range;
use storage::{HardLimits, LimitKind, Storage, StorageConfig, StorageError};
use tempfile::TempDir;

fn config(temp_dir: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: temp_dir.path().join("evorch.db"),
        ..StorageConfig::default()
    }
}

fn bucket(window_start: u64, provider: &str, input_tokens: u64) -> UsageBucket {
    UsageBucket {
        key: BucketKey {
            window_start,
            provider: provider.into(),
            model: "model".into(),
        },
        input_tokens,
        output_tokens: 2,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
        cache_hits: 5,
        cache_misses: 6,
        request_count: 1,
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

fn read_buckets(path: &std::path::Path) -> Vec<UsageBucket> {
    let connection = Connection::open(path).expect("database must reopen");
    list_range(&connection, 0, i64::MAX as u64).expect("metrics must list")
}

#[test]
fn usage_sink_submit_then_flush_persists_buckets() {
    // Given: ファイル DB と異なるキーの二バケット
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let expected = [bucket(60, "alpha", 10), bucket(120, "beta", 20)];

    // When: UsageSink 経由で投入し明示 flush する
    let sink: &dyn UsageSink = &handle;
    sink.submit(expected.to_vec());
    handle.flush_usage_now().expect("usage must flush");

    // Then: 二バケットが完全に永続化される
    assert_eq!(read_buckets(&config.db_path), expected);
}

#[test]
fn pre_flush_merge_adds_same_key_in_memory() {
    // Given: 同じキーを持つ未 flush の二バケット
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();

    // When: 二回投入してから一度だけ flush する
    handle.submit(vec![bucket(60, "alpha", 10)]);
    handle.submit(vec![bucket(60, "alpha", 5)]);
    handle.flush_usage_now().expect("usage must flush");

    // Then: 一行に全カウンターが加算される
    assert_eq!(
        read_buckets(&config.db_path),
        [bucket(60, "alpha", 15).with_multiplier(2)]
    );
}

#[test]
fn additive_refill_adds_to_existing_database_row() {
    // Given: 一度 flush 済みのバケット
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    handle.submit(vec![bucket(60, "alpha", 10)]);
    handle.flush_usage_now().expect("first usage must flush");

    // When: 同じキーを再投入して flush する
    handle.submit(vec![bucket(60, "alpha", 5)]);
    handle.flush_usage_now().expect("second usage must flush");

    // Then: 既存行へ加算される
    assert_eq!(
        read_buckets(&config.db_path),
        [bucket(60, "alpha", 15).with_multiplier(2)]
    );
}

#[test]
fn shutdown_flushes_pending_usage() {
    // Given: flush 前の一バケット
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    storage.handle().submit(vec![bucket(60, "alpha", 10)]);

    // When: Storage を明示 close する
    storage.close();

    // Then: shutdown flush により永続化される
    assert_eq!(read_buckets(&config.db_path), [bucket(60, "alpha", 10)]);
}

#[test]
fn submit_after_close_is_safe_and_requests_report_writer_closed() {
    // Given: close 後も保持した handle
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let storage = Storage::open(config(&temp_dir)).expect("storage must open");
    let handle = storage.handle();
    storage.close();

    // When: lossy submit と応答付き append を実行する
    handle.submit(vec![bucket(60, "alpha", 1)]);
    let result = handle.append_event(
        None,
        &event(
            LifecycleEvent::Completed {
                session_id: "s1".into(),
            },
            1,
        ),
    );

    // Then: submit は panic せず、append は終了を通知する
    assert_eq!(result, Err(StorageError::WriterClosed));
}

#[test]
fn append_event_end_to_end_enforces_event_and_session_limits() {
    // Given: 既定上限を超えるイベント
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let storage = Storage::open(config(&temp_dir)).expect("storage must open");
    let handle = storage.handle();
    let oversized = event(
        MessageEvent::MessageDelta {
            delta: "x".repeat(300_000),
        },
        1,
    );

    // When: oversized event を追記する
    let event_error = handle
        .append_event(None, &oversized)
        .expect_err("oversized event must fail");

    // Then: EventSize 超過になる
    assert!(matches!(
        event_error,
        StorageError::LimitExceeded {
            limit: LimitKind::EventSize,
            ..
        }
    ));
    storage.close();

    // Given: 一件だけ収まるセッション上限と既存セッション
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let mut config = config(&temp_dir);
    let small = event(
        MessageEvent::MessageDelta {
            delta: "small".into(),
        },
        2,
    );
    let payload = serde_json::to_string(&small.kind)
        .expect("event must serialize")
        .len() as u64;
    config.hard_limits.max_session_bytes = payload;
    let storage = Storage::open(config.clone()).expect("storage must open");
    let connection = Connection::open(&config.db_path).expect("database must open");
    connection.execute("INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) VALUES ('s1', 'running', 0, 0)", []).expect("session must insert");
    let handle = storage.handle();
    handle
        .append_event(Some("s1"), &small)
        .expect("first event must fit");

    // When: 同じセッションへ二件目を追記する
    let session_error = handle
        .append_event(Some("s1"), &small)
        .expect_err("session limit must fail");

    // Then: SessionSize 超過になる
    assert!(matches!(
        session_error,
        StorageError::LimitExceeded {
            limit: LimitKind::SessionSize,
            ..
        }
    ));
}

#[test]
fn db_size_suspension_rejects_events_but_allows_usage() {
    // Given: DB 本体より小さい最大サイズ
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let mut config = config(&temp_dir);
    config.hard_limits = HardLimits {
        max_db_bytes: 1,
        ..HardLimits::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open in suspended mode");
    let handle = storage.handle();

    // When: event と usage をそれぞれ投入する
    let event_result = handle.append_event(
        None,
        &event(
            LifecycleEvent::Completed {
                session_id: "s1".into(),
            },
            1,
        ),
    );
    handle.submit(vec![bucket(60, "alpha", 10)]);
    let flush_result = handle.flush_usage_now();

    // Then: event だけ DbSize 超過となり usage は保存される
    assert!(matches!(
        event_result,
        Err(StorageError::LimitExceeded {
            limit: LimitKind::DbSize,
            ..
        })
    ));
    assert_eq!(flush_result, Ok(()));
    assert_eq!(read_buckets(&config.db_path), [bucket(60, "alpha", 10)]);
}

#[test]
fn checkpoint_now_passive_preserves_reopenable_database() {
    // Given: file-backed writer による一件の書き込み
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    handle.submit(vec![bucket(60, "alpha", 10)]);
    handle.flush_usage_now().expect("usage must flush");

    // When: 明示 checkpoint して close する
    handle.checkpoint_now().expect("checkpoint must succeed");
    storage.close();

    // Then: DB を再度開いて同じ行を読める
    assert_eq!(read_buckets(&config.db_path), [bucket(60, "alpha", 10)]);
}

trait BucketTestExt {
    fn with_multiplier(self, multiplier: u64) -> Self;
}

impl BucketTestExt for UsageBucket {
    fn with_multiplier(mut self, multiplier: u64) -> Self {
        self.output_tokens *= multiplier;
        self.cache_read_tokens *= multiplier;
        self.cache_write_tokens *= multiplier;
        self.cache_hits *= multiplier;
        self.cache_misses *= multiplier;
        self.request_count *= multiplier;
        self
    }
}
