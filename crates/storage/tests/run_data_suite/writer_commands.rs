//! writer command 経由の catalog 更新と projection 再調整を検証します。

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{Event, EventMeta, LifecycleEvent};
use rusqlite::Connection;
use storage::{CatalogUpdateRecord, Database, ReconcileSummary, Storage, StorageConfig};
use tempfile::TempDir;

fn config(temp_dir: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: temp_dir.path().join("writer-commands.db"),
        ..StorageConfig::default()
    }
}

#[test]
fn record_catalog_update_roundtrips_through_writer() {
    // Given: ファイル DB の writer と一件のカタログ更新
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    let expected = CatalogUpdateRecord {
        source: "models-dev".into(),
        model_count: 3,
        detail: "refreshed".into(),
        recorded_at_ns: 100,
    };

    // When: handle 経由で記録して writer を閉じる
    storage
        .handle()
        .record_catalog_update(&expected)
        .expect("catalog update must record");
    storage.close();

    // Then: read-only 接続から同じ履歴を取得できる
    let database = Database::open(&config).expect("database must reopen");
    assert_eq!(
        database
            .catalog_updates()
            .expect("catalog updates must list"),
        [expected]
    );
}

#[test]
fn reconcile_updates_projection_rows_through_writer() {
    // Given: 開始イベントを保持する writer
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let event = Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_secs(1),
            wall_clock: UNIX_EPOCH + Duration::from_secs(1),
        },
        kind: LifecycleEvent::Started {
            session_id: "s1".into(),
        }
        .into(),
    };
    handle
        .append_event(Some("s1"), &event)
        .expect("event must append");

    // When: handle 経由で再調整する
    let summary = handle.reconcile().expect("projection must reconcile");
    storage.close();

    // Then: 対象セッション一件を upsert した結果が返る
    assert_eq!(
        summary,
        ReconcileSummary {
            sessions_upserted: 1,
            tasks_upserted: 0,
        }
    );
    let connection = Connection::open(&config.db_path).expect("database must reopen");
    let session_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions WHERE id = 's1'", [], |row| {
            row.get(0)
        })
        .expect("session count must read");
    assert_eq!(session_count, 1);
}
