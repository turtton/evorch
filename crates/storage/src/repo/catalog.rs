//! カタログ更新履歴を永続化します。

use rusqlite::{Connection, params};

use crate::{CatalogUpdateRecord, StorageError};

/// カタログ更新履歴を保存します。
///
/// # Errors
///
/// SQLite 操作に失敗した場合にエラーを返します。
pub fn record(conn: &Connection, record: &CatalogUpdateRecord) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO catalog_updates (source, model_count, detail, recorded_at_ns) VALUES (?1, ?2, ?3, ?4)",
        params![
            &record.source,
            to_i64(record.model_count),
            &record.detail,
            record.recorded_at_ns,
        ],
    )?;
    Ok(())
}

/// カタログ更新履歴を自動採番 ID の昇順で返します。
///
/// # Errors
///
/// SQLite 操作または保存済みモデル数の変換に失敗した場合にエラーを返します。
pub fn list(conn: &Connection) -> Result<Vec<CatalogUpdateRecord>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT source, model_count, detail, recorded_at_ns FROM catalog_updates ORDER BY id ASC",
    )?;
    let mut rows = statement.query([])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        records.push(CatalogUpdateRecord {
            source: row.get(0)?,
            model_count: from_i64(row.get(1)?)?,
            detail: row.get(2)?,
            recorded_at_ns: row.get(3)?,
        });
    }
    Ok(records)
}

fn to_i64(value: u32) -> i64 {
    i64::from(value)
}

fn from_i64(value: i64) -> Result<u32, StorageError> {
    u32::try_from(value).map_err(|_| StorageError::OutOfRange("catalog update model count"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CatalogUpdateRecord, Database};

    fn catalog_update_record(
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
    fn migration_creates_catalog_updates_table() {
        // Given: 移行前の空のメモリ上データベース
        let database = Database::open_in_memory().expect("database must open");

        // When: 最新の移行を適用して開く
        let exists = database
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'catalog_updates')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("catalog_updates table query must succeed");

        // Then: カタログ更新履歴テーブルが作成される
        assert!(exists);
    }

    #[test]
    fn record_and_list_catalog_updates_roundtrip() {
        // Given: 一件のカタログ更新履歴
        let database = Database::open_in_memory().expect("database must open");
        let expected = catalog_update_record("provider-a", 2, "initial catalog", 10);

        // When: 履歴を保存して一覧を取得する
        record(&database.conn, &expected).expect("catalog update must record");
        let actual = list(&database.conn).expect("catalog updates must list");

        // Then: 全フィールドが同じ値で復元される
        assert_eq!(actual, [expected]);
    }

    #[test]
    fn catalog_updates_listed_in_insertion_order() {
        // Given: 記録時刻とは異なる順で保存する三件のカタログ更新履歴
        let database = Database::open_in_memory().expect("database must open");
        let records = [
            catalog_update_record("provider-a", 3, "first", 30),
            catalog_update_record("provider-b", 1, "second", 10),
            catalog_update_record("provider-c", 2, "third", 20),
        ];

        // When: 履歴を配列順に保存して一覧を取得する
        for item in &records {
            record(&database.conn, item).expect("catalog update must record");
        }
        let actual = list(&database.conn).expect("catalog updates must list");

        // Then: 自動採番 ID による挿入順で返る
        assert_eq!(actual, records);
    }
}
