//! SQLite スキーマ移行と接続設定の統合テスト。

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::Connection;
use storage::{Database, StorageConfig, StorageError};
use tempfile::TempDir;

const EXPECTED_TABLES: [&str; 19] = [
    "run_ledger",
    "run_contexts",
    "team_ledger",
    "task_queue_ledger",
    "memory_ledger",
    "memory_entries",
    "memory_fts",
    "memory_fts_data",
    "memory_fts_idx",
    "memory_fts_docsize",
    "memory_fts_config",
    "task_links",
    "agent_runs",
    "catalog_updates",
    "downsampled_metrics",
    "events",
    "messages",
    "sessions",
    "tasks",
];

const EXPECTED_INDICES: [&str; 12] = [
    "idx_run_ledger_run",
    "idx_eval_trace_identity",
    "idx_eval_trace_project",
    "idx_memory_ledger_entry",
    "idx_memory_entries_project_status",
    "idx_task_links_blocked",
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

    // Then: v7 と定義済みテーブル・インデックスが作成される
    let connection = Connection::open(path).expect("migrated database must reopen");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("user_version must be readable"),
        7
    );
    assert_eq!(
        schema_objects(&connection, "table"),
        EXPECTED_TABLES.into_iter().map(String::from).collect()
    );
    assert_eq!(
        schema_objects(&connection, "index"),
        EXPECTED_INDICES.into_iter().map(String::from).collect()
    );
    assert!(schema_objects(&connection, "trigger").contains("run_ledger_no_replace"));
}

#[test]
fn reopening_latest_database_is_idempotent() {
    // Given: 最新版へ移行済みのデータベース
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let path = database_path(&temp_dir);
    drop(Database::open(&config_for(&path)).expect("fresh database must open"));

    // When: 同じファイルを再度開く
    drop(Database::open(&config_for(&path)).expect("migrated database must reopen"));

    // Then: スキーマは重複せず v7 のまま維持される
    let connection = Connection::open(path).expect("database must remain readable");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("user_version must be readable"),
        7
    );
    assert_eq!(
        schema_objects(&connection, "table").len(),
        EXPECTED_TABLES.len()
    );
    assert_eq!(
        schema_objects(&connection, "index").len(),
        EXPECTED_INDICES.len()
    );
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
            supported: 7,
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

#[test]
fn v2_upgrade_preserves_existing_tasks_and_events() {
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    let connection = Connection::open(&path).unwrap();
    let sql = include_str!("../src/migrations/sql.rs");
    for migration in sql.split("r#\"").skip(1) {
        connection
            .execute_batch(migration.split("\"#;").next().unwrap())
            .unwrap();
    }
    connection.pragma_update(None, "user_version", 2).unwrap();
    connection
        .execute(
            "INSERT INTO tasks VALUES('existing',NULL,'running',10,20)",
            [],
        )
        .unwrap();
    drop(connection);
    let database = Database::open(&config_for(&path)).unwrap();
    assert_eq!(
        database.task("existing").unwrap().unwrap().status,
        storage::entity::TaskStatus::Running
    );
    assert_eq!(database.pragma_i64("user_version").unwrap(), 7);
}

#[test]
fn v6_upgrade_protects_existing_ledger_rows_from_replace() {
    // Given: the shipped v6 ledger schema with a persisted row.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("../src/migrations/v6.sql"))
        .unwrap();
    connection.pragma_update(None, "user_version", 6).unwrap();
    connection
        .execute("INSERT INTO run_ledger VALUES (1, 'a', 'original', 10)", [])
        .unwrap();
    drop(connection);

    // When: opening the existing database applies v7.
    let database = Database::open(&config_for(&path)).unwrap();

    // Then: the migrated row is protected even without recursive triggers.
    assert_eq!(database.pragma_i64("user_version").unwrap(), 7);
    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "recursive_triggers", 0)
        .unwrap();
    let error = connection
        .execute("REPLACE INTO run_ledger VALUES (1, 'b', 'changed', 20)", [])
        .unwrap_err();
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER)
    );
    let entries = database.run_ledger_all().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (
            entries[0].seq,
            entries[0].run_id.as_str(),
            entries[0].body.as_str(),
            entries[0].created_at_ns
        ),
        (1, "a", "original", 10)
    );
}
