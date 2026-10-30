//! SQLite スキーマ移行と接続設定の統合テスト。

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::Connection;
use storage::{Database, StorageConfig, StorageError};
use tempfile::TempDir;

const LATEST_SCHEMA: u32 = 32;

const EXPECTED_TABLES: [&str; 50] = [
    "learning_claims",
    "learning_claim_roots",
    "run_context_provenance",
    "thread_index_runs",
    "conversation_blocks",
    "conversation_fts",
    "conversation_fts_data",
    "conversation_fts_idx",
    "conversation_fts_config",
    "thread_index_state",
    "thread_index_lease",
    "thread_index_requests",
    "thread_index_request_targets",
    "host_input_receipts",
    "threads",
    "thread_runs",
    "eval_samples",
    "thread_start_states",
    "thread_start_untracked",
    "assistant_references",
    "review_evidence",
    "review_lineage_edges",
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

const EXPECTED_INDICES: [&str; 49] = [
    "learning_claims_day",
    "learning_claim_roots_run",
    "idx_thread_index_runs_work",
    "idx_thread_index_runs_retention",
    "idx_thread_index_runs_retry",
    "idx_conversation_blocks_kind",
    "idx_thread_index_request_targets_run",
    "idx_events_durable_retention",
    "idx_events_durable_retention_scan",
    "idx_events_task_lifecycle",
    "host_input_receipts_run",
    "host_input_receipts_seq",
    "idx_threads_project_updated",
    "idx_threads_parent",
    "idx_thread_runs_thread",
    "idx_run_contexts_parent",
    "idx_usage_requests_parent",
    "idx_thread_start_states_captured",
    "review_evidence_id",
    "review_evidence_target",
    "review_evidence_root",
    "review_lineage_edges_parent",
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
    "idx_events_kind_subkind_id",
    "idx_events_run_id",
    "idx_events_unbackfilled",
    "idx_events_blank_subkind",
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

    // Then: 最新版の定義済みテーブル・インデックスが作成される
    let connection = Connection::open(path).expect("migrated database must reopen");
    assert_eq!(
        database.pragma_i64("user_version").unwrap(),
        i64::from(LATEST_SCHEMA)
    );
    assert_eq!(
        schema_objects(&connection, "table"),
        EXPECTED_TABLES.into_iter().map(String::from).collect()
    );
    assert_eq!(
        schema_objects(&connection, "index"),
        EXPECTED_INDICES.into_iter().map(String::from).collect()
    );
    let triggers = schema_objects(&connection, "trigger");
    assert!(triggers.contains("run_ledger_no_replace"));
    assert!(triggers.contains("thread_start_untracked_replayable"));
    assert!(triggers.contains("eval_samples_replayable"));
}

#[test]
fn reopening_latest_database_is_idempotent() {
    // Given: 最新版へ移行済みのデータベース
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let path = database_path(&temp_dir);
    drop(Database::open(&config_for(&path)).expect("fresh database must open"));

    // When: 同じファイルを再度開く
    drop(Database::open(&config_for(&path)).expect("migrated database must reopen"));

    // Then: スキーマは重複せず最新版のまま維持される
    let connection = Connection::open(path).expect("database must remain readable");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("user_version must be readable"),
        LATEST_SCHEMA
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
            supported: LATEST_SCHEMA,
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
    assert_eq!(
        database.pragma_i64("user_version").unwrap(),
        i64::from(LATEST_SCHEMA)
    );
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
    assert_eq!(
        database.pragma_i64("user_version").unwrap(),
        i64::from(LATEST_SCHEMA)
    );
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
    assert_eq!(
        database.pragma_i64("user_version").unwrap(),
        i64::from(LATEST_SCHEMA)
    );
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
        "attempts",
        "heartbeat_at_ns",
    ] {
        assert!(columns.contains(column), "missing {column}");
    }
    assert!(!columns.contains("resume_cursor_json"));
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
            "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             DROP TABLE threads; DROP TABLE thread_runs;
             DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
             ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
             ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
             ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
             DROP TABLE IF EXISTS eval_samples;
             DROP TABLE IF EXISTS thread_start_untracked;
             DROP TABLE IF EXISTS thread_start_states;
             DROP TABLE IF EXISTS assistant_references;
             DROP TABLE IF EXISTS review_evidence;
             DROP TABLE IF EXISTS review_lineage_edges;
             ALTER TABLE usage_requests DROP COLUMN service_tier;
             ALTER TABLE improvement_candidates DROP COLUMN occurrences;
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

#[test]
fn v17_adds_review_storage_to_v16_without_changing_existing_data() {
    // Given: a v16 database with existing data and no review tables.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(
        "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             DROP TABLE threads; DROP TABLE thread_runs;
         DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
         ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
         DROP TABLE IF EXISTS eval_samples;
         DROP TABLE IF EXISTS thread_start_untracked;
         DROP TABLE IF EXISTS thread_start_states;
         DROP TABLE IF EXISTS assistant_references;
         DROP TABLE IF EXISTS review_evidence;
         DROP TABLE IF EXISTS review_lineage_edges;
         ALTER TABLE usage_requests DROP COLUMN service_tier;
         INSERT INTO run_ledger(run_id, body, created_at_ns) VALUES ('existing', 'keep me', 7);
         INSERT INTO run_contexts VALUES ('existing', 'worker', 'name', NULL, '{}', '[]', '[]', 'Done', 1, 9);"
    ).unwrap();
    connection.pragma_update(None, "user_version", 16).unwrap();
    drop(connection);

    // When: opening applies v17 and reopening is idempotent.
    for _ in 0..2 {
        let database = Database::open(&config_for(&path)).unwrap();
        assert_eq!(
            database.pragma_i64("user_version").unwrap(),
            i64::from(LATEST_SCHEMA)
        );
        assert_eq!(database.run_ledger("existing").unwrap()[0].body, "keep me");
        assert_eq!(
            database.run_context("existing").unwrap().unwrap().name,
            "name"
        );
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            schema_objects(&connection, "table"),
            EXPECTED_TABLES.into_iter().map(String::from).collect()
        );
        assert_eq!(
            schema_objects(&connection, "index"),
            EXPECTED_INDICES.into_iter().map(String::from).collect()
        );
    }
}

#[test]
fn v18_adds_assistant_references_without_rewriting_note_revisions() {
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(
        "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             DROP TABLE threads; DROP TABLE thread_runs;
         DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
         ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
         DROP TABLE eval_samples;
         DROP TABLE thread_start_untracked;
         DROP TABLE thread_start_states;
         DROP TABLE assistant_references;
         ALTER TABLE usage_requests DROP COLUMN service_tier;
         INSERT INTO run_ledger(run_id, body, created_at_ns) VALUES ('existing', 'complete note', 7);"
    ).unwrap();
    connection.pragma_update(None, "user_version", 17).unwrap();
    drop(connection);
    let database = Database::open(&config_for(&path)).unwrap();
    assert_eq!(
        database.pragma_i64("user_version").unwrap(),
        i64::from(LATEST_SCHEMA)
    );
    assert_eq!(
        database
            .latest_run_ledger("existing")
            .unwrap()
            .unwrap()
            .body,
        "complete note"
    );
    assert!(database.assistant_reference("missing").unwrap().is_none());
}

#[test]
fn v20_adds_thread_start_storage_without_changing_existing_data() {
    // Given: a v19 database with existing data and no thread start tables.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             DROP TABLE threads; DROP TABLE thread_runs;
             DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
             ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
             ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
             ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
             DROP TABLE eval_samples;
             DROP TABLE thread_start_untracked;
             DROP TABLE thread_start_states;
             INSERT INTO run_ledger(run_id, body, created_at_ns) VALUES ('existing', 'keep me', 7);",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 19).unwrap();
    drop(connection);

    // When: opening applies v20 and reopening is idempotent.
    for _ in 0..2 {
        let database = Database::open(&config_for(&path)).unwrap();

        // Then: earlier data stays and the thread start schema is complete.
        assert_eq!(
            database.pragma_i64("user_version").unwrap(),
            i64::from(LATEST_SCHEMA)
        );
        assert_eq!(database.run_ledger("existing").unwrap()[0].body, "keep me");
        assert!(database.thread_start_state("missing").unwrap().is_none());
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            schema_objects(&connection, "table"),
            EXPECTED_TABLES.into_iter().map(String::from).collect()
        );
        assert_eq!(
            schema_objects(&connection, "index"),
            EXPECTED_INDICES.into_iter().map(String::from).collect()
        );
    }
}

#[test]
fn v21_adds_head_ref_and_keeps_earlier_thread_starts_without_one() {
    // Given: a v20 database holding a replayable thread start without the head ref column.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             DROP TABLE threads; DROP TABLE thread_runs;
             DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
             ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
             ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
             ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
             DROP TABLE eval_samples;
             ALTER TABLE thread_start_states DROP COLUMN head_ref;
             INSERT INTO thread_start_states (thread_id, root_run_id, project_root, repo_prefix, \
             head_commit, diff, captured_at_ns, replayable, reason) \
             VALUES ('old', 'run-1', '/repo', '', 'abc', x'', 7, 1, NULL);",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 20).unwrap();
    drop(connection);

    // When: opening applies v21 and reopening is idempotent.
    for _ in 0..2 {
        let database = Database::open(&config_for(&path)).unwrap();

        // Then: the earlier start reads back with an unknown head ref.
        assert_eq!(
            database.pragma_i64("user_version").unwrap(),
            i64::from(LATEST_SCHEMA)
        );
        let state = database.thread_start_state("old").unwrap().unwrap();
        let storage::thread_start::ThreadStartCapture::Replayable(snapshot) = state.capture else {
            panic!("expected a replayable start");
        };
        assert_eq!(snapshot.head_commit, "abc");
        assert_eq!(snapshot.head_ref, None);
    }
}

#[test]
fn v22_adds_eval_samples_without_changing_thread_starts() {
    // Given: a v21 database holding a replayable thread start and no sample table.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             DROP TABLE threads; DROP TABLE thread_runs;
             DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
             ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
             ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
             ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
             DROP TABLE eval_samples;
             INSERT INTO thread_start_states (thread_id, root_run_id, project_root, repo_prefix, \
             head_commit, diff, captured_at_ns, replayable, reason) \
             VALUES ('old', 'run-1', '/repo', '', 'abc', x'', 7, 1, NULL);",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 21).unwrap();
    drop(connection);

    // When: opening applies v22 and reopening is idempotent.
    for _ in 0..2 {
        let database = Database::open(&config_for(&path)).unwrap();

        // Then: the start stays and the sample schema is complete.
        assert_eq!(
            database.pragma_i64("user_version").unwrap(),
            i64::from(LATEST_SCHEMA)
        );
        assert!(database.thread_start_state("old").unwrap().is_some());
        assert!(database.eval_samples().unwrap().is_empty());
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            schema_objects(&connection, "table"),
            EXPECTED_TABLES.into_iter().map(String::from).collect()
        );
        assert!(schema_objects(&connection, "trigger").contains("eval_samples_replayable"));
    }
}

#[test]
fn v23_adds_reopening_metadata_and_drops_task_resume_cursor() {
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(
        "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             DROP TABLE threads; DROP TABLE thread_runs;
         DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
         ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
         INSERT INTO improvement_candidates
           (candidate_id, project, created_at_ns, source, code, severity, title, evidence, dedup_key)
         VALUES ('existing', 'p', 7, 'diagnostic', 'NoProgress', 'warning', 'title', 'evidence', 'key');
         INSERT INTO tasks (id, status, created_at_ns, updated_at_ns, resume_cursor_json)
         VALUES ('task', 'running', 1, 2, 'legacy-cursor');"
    ).unwrap();
    connection.pragma_update(None, "user_version", 22).unwrap();
    drop(connection);
    for _ in 0..2 {
        let database = Database::open(&config_for(&path)).unwrap();
        assert_eq!(
            database.pragma_i64("user_version").unwrap(),
            i64::from(LATEST_SCHEMA)
        );
        let candidate = database.improvement_candidate("existing").unwrap().unwrap();
        assert_eq!(candidate.reopened_count, 0);
        assert_eq!(candidate.reopened_at_ns, None);
        assert_eq!(candidate.evidence, "evidence");
        let connection = Connection::open(&path).unwrap();
        let columns: BTreeSet<String> = connection
            .prepare("PRAGMA table_info(tasks)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!columns.contains("resume_cursor_json"));
        let task: (String, i64, i64) = connection
            .query_row(
                "SELECT status, created_at_ns, updated_at_ns FROM tasks WHERE id = 'task'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(task, ("running".into(), 1, 2));
        let reopened_column: (String, i64, String) = connection.query_row(
            "SELECT type, [notnull], dflt_value FROM pragma_table_info('improvement_candidates') WHERE name = 'reopened_count'", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        assert_eq!(reopened_column, ("INTEGER".into(), 1, "0".into()));
        let timestamp_column: (String, i64) = connection.query_row(
            "SELECT type, [notnull] FROM pragma_table_info('improvement_candidates') WHERE name = 'reopened_at_ns'", [],
            |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(timestamp_column, ("INTEGER".into(), 0));
    }
}

#[test]
fn v24_adds_threads_backfilled_from_v23_runs_and_read_only_needs_the_upgrade() {
    // Given: a v23 database holding a named conversation root and its child.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             DROP TABLE threads; DROP TABLE thread_runs;
             DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
             INSERT INTO run_contexts VALUES ('run-1', 'worker', 'chat:Worker:t-1', NULL,
                 '{\"project_root\":\"/repo\"}', '[]', '[]', 'Done', 1, 7);
             INSERT INTO run_contexts VALUES ('run-2', 'explore', 'explore', 'run-1',
                 '{}', '[]', '[]', 'Done', 1, 9);",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 23).unwrap();
    drop(connection);

    // When: a read-only open is attempted before the upgrade.
    let error = Database::open_read_only(&path).unwrap_err();
    // Then: it refuses the older schema without migrating it.
    assert!(
        matches!(&error, StorageError::Serialization(message) if message.contains(&format!("older than {LATEST_SCHEMA}"))),
        "{error:?}"
    );

    // When: the current schema opens it, twice.
    for _ in 0..2 {
        let database = Database::open(&config_for(&path)).unwrap();
        assert_eq!(
            database.pragma_i64("user_version").unwrap(),
            i64::from(LATEST_SCHEMA)
        );
    }

    // Then: the schema is complete, read-only access works and the runs are a thread.
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        schema_objects(&connection, "table"),
        EXPECTED_TABLES.into_iter().map(String::from).collect()
    );
    assert_eq!(
        schema_objects(&connection, "index"),
        EXPECTED_INDICES.into_iter().map(String::from).collect()
    );
    let database = Database::open_read_only(&path).unwrap();
    let thread = database.thread("t-1").unwrap().unwrap();
    assert_eq!(
        (
            thread.project_root.as_deref(),
            thread.root_run_id.as_deref()
        ),
        (Some("/repo"), Some("run-1"))
    );
    assert_eq!(
        database
            .thread_of_run("run-2")
            .unwrap()
            .map(|run| run.depth),
        Some(1)
    );
}

#[test]
fn v25_adds_host_input_receipts_and_keeps_v24_run_contexts() {
    // Given: a v24 database holding a saved run context.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
             DROP TABLE host_input_receipts;
             ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
             INSERT INTO run_contexts VALUES ('run-1', 'worker', 'chat:Worker:t-1', NULL,
                 '{}', '[]', '[]', 'Done', 1, 7);",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 24).unwrap();
    drop(connection);

    // When: the current schema opens it, twice.
    for _ in 0..2 {
        let database = Database::open(&config_for(&path)).unwrap();
        assert_eq!(
            database.pragma_i64("user_version").unwrap(),
            i64::from(LATEST_SCHEMA)
        );
        // Then: the old snapshot reads with no host inputs and no receipts.
        let record = database.run_context("run-1").unwrap().unwrap();
        assert_eq!(record.host_inputs_json, "[]");
        assert!(
            database
                .host_input_receipts_for_run("run-1")
                .unwrap()
                .is_empty()
        );
    }
    let connection = Connection::open(&path).unwrap();
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
fn failed_v25_migration_rolls_back_its_ddl_and_keeps_the_version() {
    // Given: a v24 database whose run_contexts already has the v25 column, so
    // the migration's last statement fails after creating the receipt table.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("DROP TABLE host_input_receipts;")
        .unwrap();
    connection.pragma_update(None, "user_version", 24).unwrap();
    drop(connection);

    // When: opening applies v25.
    let Err(error) = Database::open(&config_for(&path)) else {
        panic!("v25 must fail");
    };

    // Then: the error names v25 and none of its DDL or the version bump remain.
    assert!(
        matches!(&error, StorageError::Migration { version: 25, .. }),
        "{error:?}"
    );
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        24
    );
    assert!(!schema_objects(&connection, "table").contains("host_input_receipts"));
    assert!(!schema_objects(&connection, "index").contains("host_input_receipts_run"));
}

#[test]
fn v31_records_plaintext_lengths_of_existing_snapshots() {
    // Given: a v30 database holding a plain snapshot with multibyte text.
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("v30.db");
    drop(Database::open(&config_for(&path)).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE review_evidence DROP COLUMN attachments_json;
             DROP INDEX host_input_receipts_seq;
             ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq;
             ALTER TABLE host_input_receipts DROP COLUMN evidence_id;
             ALTER TABLE run_contexts DROP COLUMN messages_len;
             ALTER TABLE run_contexts DROP COLUMN checkpoints_len;
             INSERT INTO run_contexts(run_id, role, name, config_json, messages_json,
                 checkpoints_json, terminal_phase, restorable, updated_at_ns)
             VALUES ('run-old', 'worker', 'n', '{}', '[\"こんにちは\"]', '[]', 'Done', 1, 1);",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 30).unwrap();
    drop(connection);

    // When: the current schema opens it.
    drop(Database::open(&config_for(&path)).unwrap());

    // Then: the recorded lengths are byte lengths, and the row still reads as before.
    let connection = Connection::open(&path).unwrap();
    let lengths: (i64, i64) = connection
        .query_row(
            "SELECT messages_len, checkpoints_len FROM run_contexts WHERE run_id = 'run-old'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        lengths,
        (i64::try_from("[\"こんにちは\"]".len()).unwrap(), 2)
    );
    let database = Database::open(&config_for(&path)).unwrap();
    let record = database.run_context("run-old").unwrap().unwrap();
    assert_eq!(record.messages_json, "[\"こんにちは\"]");
}
