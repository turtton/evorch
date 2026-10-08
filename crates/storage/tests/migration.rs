//! SQLite スキーマ移行と接続設定の統合テスト。

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::Connection;
use storage::{Database, StorageConfig, StorageError};
use tempfile::TempDir;

const EXPECTED_TABLES: [&str; 28] = [
    "usage_requests",
    "usage_run_threads",
    "usage_daily",
    "improvement_candidates",
    "improvement_intake",
    "user_questions",
    "user_question_links",
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
    "event_session_bytes",
    "event_day_bytes",
    "messages",
    "sessions",
    "tasks",
];

const EXPECTED_INDICES: [&str; 24] = [
    "idx_usage_requests_at",
    "idx_usage_requests_run",
    "idx_improvement_intake_dedup",
    "idx_improvement_intake_daily",
    "idx_improvement_candidates_dedup",
    "idx_improvement_candidates_status",
    "user_questions_run",
    "user_questions_root",
    "user_questions_pending",
    "user_question_links_run",
    "idx_run_ledger_run",
    "idx_eval_trace_identity",
    "idx_eval_trace_project",
    "idx_memory_ledger_entry",
    "idx_memory_entries_project_status",
    "idx_memory_entries_scope_status",
    "idx_task_links_blocked",
    "idx_agent_runs_session_id",
    "idx_events_session_id",
    "idx_events_wall_clock",
    "idx_events_diagnostic_retention",
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
    let database = Database::open(&config_for(&path)).expect("fresh database must open");

    // Then: v16 と定義済みテーブル・インデックスが作成される
    let connection = Connection::open(path).expect("migrated database must reopen");
    assert_eq!(database.pragma_i64("user_version").unwrap(), 16);
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

    // Then: スキーマは重複せず v16 のまま維持される
    let connection = Connection::open(path).expect("database must remain readable");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("user_version must be readable"),
        16
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
            supported: 16,
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
    assert_eq!(database.pragma_i64("user_version").unwrap(), 16);
}

#[test]
fn v6_upgrade_protects_existing_ledger_rows_from_replace() {
    // Given: the shipped v6 ledger schema with a persisted row.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    let connection = Connection::open(&path).unwrap();
    for migration in include_str!("../src/migrations/sql.rs")
        .split("r#\"")
        .skip(1)
    {
        connection
            .execute_batch(migration.split("\"#;").next().unwrap())
            .unwrap();
    }
    for migration in [
        include_str!("../src/migrations/v3.sql"),
        include_str!("../src/migrations/v4.sql"),
        include_str!("../src/migrations/v5.sql"),
    ] {
        connection.execute_batch(migration).unwrap();
    }
    connection
        .execute_batch(include_str!("../src/migrations/v6.sql"))
        .unwrap();
    connection.pragma_update(None, "user_version", 6).unwrap();
    connection
        .execute("INSERT INTO run_ledger VALUES (1, 'a', 'original', 10)", [])
        .unwrap();
    drop(connection);

    // When: opening the existing database applies the remaining migrations.
    let database = Database::open(&config_for(&path)).unwrap();

    // Then: the migrated row is protected even without recursive triggers.
    assert_eq!(database.pragma_i64("user_version").unwrap(), 16);
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

#[test]
fn v8_extends_tasks_with_durable_columns_and_widened_check() {
    // Given: a fresh database path.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    // When: opening migrates to the latest schema.
    let database = Database::open(&config_for(&path)).unwrap();
    let connection = Connection::open(&path).unwrap();
    // Then: all durable columns and statuses are supported.
    assert_eq!(database.pragma_i64("user_version").unwrap(), 16);
    let columns: BTreeSet<String> = connection
        .prepare("PRAGMA table_info(tasks)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for column in [
        "parent_run_id",
        "input_json",
        "progress_json",
        "last_artifact_json",
        "failure_reason",
        "resume_cursor_json",
        "attempts",
        "heartbeat_at_ns",
    ] {
        assert!(columns.contains(column), "missing {column}");
    }
    for status in [
        "pending",
        "queued",
        "running",
        "blocked",
        "retrying",
        "completed",
        "cancelled",
        "failed",
    ] {
        connection
            .execute(
                "INSERT INTO tasks(id,status,created_at_ns,updated_at_ns) VALUES(?1,?1,0,0)",
                [status],
            )
            .unwrap();
        assert_eq!(
            database.task(status).unwrap().unwrap().status.as_str(),
            status
        );
    }
    assert!(connection.execute("INSERT INTO tasks(id,status,created_at_ns,updated_at_ns) VALUES('bad','unknown',0,0)", []).is_err());
}

#[path = "migration/durable.rs"]
mod durable;

#[path = "migration/stopped.rs"]
mod stopped;

#[path = "migration/accounting.rs"]
mod accounting;

#[test]
fn v16_backfills_candidate_occurrence_tracking() {
    // Given: a v15 database holding candidates with and without a run.
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("v15.db");
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE improvement_candidates DROP COLUMN occurrences;
             ALTER TABLE improvement_candidates DROP COLUMN last_seen_at_ns;
             ALTER TABLE improvement_candidates DROP COLUMN recent_run_ids;
             INSERT INTO improvement_candidates
               (candidate_id, project, created_at_ns, source, code, severity,
                title, evidence, dedup_key, run_id)
             VALUES ('with-run', 'p', 7, 'diagnostic', 'NoProgress', 'warning', 't', 'e', 'k1', 'run-1'),
                    ('no-run', 'p', 9, 'lesson', 'LessonPromoted', 'info', 't', 'e', 'k2', NULL);",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 15).unwrap();
    drop(connection);

    // When: the current schema opens it.
    let database = Database::open(&config_for(&path)).unwrap();

    // Then: each candidate counts once, was last seen at creation and lists its run.
    let with_run = database.improvement_candidate("with-run").unwrap().unwrap();
    assert_eq!(
        (
            with_run.occurrences,
            with_run.last_seen_at_ns,
            with_run.recent_run_ids
        ),
        (1, 7, vec!["run-1".to_owned()])
    );
    let no_run = database.improvement_candidate("no-run").unwrap().unwrap();
    assert_eq!(
        (
            no_run.occurrences,
            no_run.last_seen_at_ns,
            no_run.recent_run_ids
        ),
        (1, 9, Vec::<String>::new())
    );
}
