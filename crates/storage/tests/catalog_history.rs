//! カタログ更新履歴の統合テスト。

use storage::{CatalogUpdateRecord, Database, Storage, StorageConfig};
use tempfile::TempDir;

fn record(
    source: &str,
    model_count: u32,
    detail: &str,
    recorded_at_ns: i64,
) -> CatalogUpdateRecord {
    CatalogUpdateRecord {
        source: source.into(),
        model_count,
        detail: detail.into(),
        recorded_at_ns,
    }
}

#[test]
fn database_catalog_update_history_roundtrip() {
    // Given: 二件のカタログ更新履歴を保存できるファイルDB writer
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = StorageConfig {
        db_path: temp_dir.path().join("catalog-history.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let records = [
        record("remote", 4, "refreshed", 100),
        record("cache", 2, "restored", 200),
    ];

    // When: 公開 API から履歴を保存して取得する
    for item in &records {
        handle
            .record_catalog_update(item)
            .expect("catalog update must record");
    }
    storage.close();
    let database = Database::open(&config).expect("database must reopen");
    let actual = database
        .catalog_updates()
        .expect("catalog updates must list");

    // Then: 保存値が挿入順で返る
    assert_eq!(actual, records);
}
