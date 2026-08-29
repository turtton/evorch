//! SQLite スキーマ移行と接続設定の統合テスト。

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::Connection;
use storage::{Database, StorageConfig, StorageError};
use tempfile::TempDir;

const EXPECTED_TABLES: [&str; 7] = [
    "agent_runs",
    "catalog_updates",
    "downsampled_metrics",
    "events",
    "messages",
    "sessions",
    "tasks",
];

const EXPECTED_INDICES: [&str; 6] = [
    "idx_agent_runs_session_id",
    "idx_events_session_id",
    "idx_events_wall_clock",
    "idx_messages_session_created",
    "idx_sessions_status",
    "idx_tasks_session_id",
];

fn database_path(temp_dir: &TempDir) -> std::path::PathBuf {
    temp_dir.path().join("storage.db")
}

fn config_for(path: &Path) -> StorageConfig {
    StorageConfig {
        db_path: path.to_path_buf(),
        ..StorageConfig::default()
    }
}

fn schema_objects(connection: &Connection, object_type: &str) -> BTreeSet<String> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = ?1 AND name NOT LIKE 'sqlite_%'")
        .expect("sqlite_master query must prepare");
    statement
        .query_map([object_type], |row| row.get(0))
        .expect("sqlite_master query must execute")
        .collect::<Result<_, _>>()
        .expect("schema names must decode")
}

#[test]
fn fresh_open_applies_latest_schema() {
    // Given: 空の一時ディレクトリに置くデータベースパス
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let path = database_path(&temp_dir);

    // When: データベースを初めて開く
    drop(Database::open(&config_for(&path)).expect("fresh database must open"));

    // Then: v2 と定義済みテーブル・インデックスだけが作成される
    let connection = Connection::open(path).expect("migrated database must reopen");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("user_version must be readable"),
        2
    );
    assert_eq!(
        schema_objects(&connection, "table"),
        EXPECTED_TABLES.into_iter().map(String::from).collect()
    );
    assert_eq!(
        schema_objects(&connection, "index"),
        EXPECTED_INDICES.into_iter().map(String::from).collect()
    );
}

#[test]
fn reopening_latest_database_is_idempotent() {
    // Given: 最新版へ移行済みのデータベース
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let path = database_path(&temp_dir);
    drop(Database::open(&config_for(&path)).expect("fresh database must open"));

    // When: 同じファイルを再度開く
    drop(Database::open(&config_for(&path)).expect("migrated database must reopen"));

    // Then: スキーマは重複せず v2 のまま維持される
    let connection = Connection::open(path).expect("database must remain readable");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("user_version must be readable"),
        2
    );
    assert_eq!(schema_objects(&connection, "table").len(), 7);
    assert_eq!(schema_objects(&connection, "index").len(), 6);
}

#[test]
fn newer_schema_version_is_rejected() {
    // Given: サポート外の user_version を持つデータベース
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let path = database_path(&temp_dir);
    let connection = Connection::open(&path).expect("raw database must open");
    connection
        .pragma_update(None, "user_version", 99_u32)
        .expect("user_version must be writable");
    drop(connection);

    // When: ストレージ層から開く
    let error = Database::open(&config_for(&path)).expect_err("newer schema must fail");

    // Then: 検出値と対応可能値を含むエラーになる
    assert_eq!(
        error,
        StorageError::SchemaTooNew {
            found: 99,
            supported: 2,
        }
    );
}

#[test]
fn open_initializes_required_pragmas() {
    // Given: 新規データベースの設定
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let path = database_path(&temp_dir);

    // When: ストレージ接続を開く
    let database = Database::open(&config_for(&path)).expect("database must open");

    // Then: 接続単位の必須 PRAGMA が設定される
    assert_eq!(database.pragma_string("journal_mode").unwrap(), "wal");
    assert_eq!(database.pragma_i64("synchronous").unwrap(), 1);
    assert_eq!(database.pragma_i64("wal_autocheckpoint").unwrap(), 1_000);
    assert_eq!(database.pragma_i64("foreign_keys").unwrap(), 1);
}
