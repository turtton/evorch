//! カタログ更新履歴の統合テスト。

use storage::{CatalogUpdateRecord, Database};

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
    // Given: 二件のカタログ更新履歴を保存できるメモリ上データベース
    let database = Database::open_in_memory().expect("database must open");
    let records = [
        record("remote", 4, "refreshed", 100),
        record("cache", 2, "restored", 200),
    ];

    // When: 公開 API から履歴を保存して取得する
    for item in &records {
        database
            .record_catalog_update(item)
            .expect("catalog update must record");
    }
    let actual = database
        .catalog_updates()
        .expect("catalog updates must list");

    // Then: 保存値が挿入順で返る
    assert_eq!(actual, records);
}
