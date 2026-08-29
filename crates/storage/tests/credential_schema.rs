//! ADR 0008 の credential 非永続化 — スキーマ構造を検証します。

use rusqlite::Connection;
use storage::{Database, StorageConfig};
use tempfile::TempDir;

type Column = (String, String, i64, Option<String>, i64);

const TABLES: [&str; 6] = [
    "sessions",
    "tasks",
    "messages",
    "agent_runs",
    "events",
    "downsampled_metrics",
];

fn open_schema() -> (TempDir, Connection) {
    let temp = TempDir::new().expect("temporary directory must be created");
    let path = temp.path().join("credential.db");
    let config = StorageConfig {
        db_path: path.clone(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).expect("database must open"));
    let connection = Connection::open(path).expect("database must reopen");
    (temp, connection)
}

fn columns(connection: &Connection, table: &str) -> Vec<Column> {
    let sql = format!(
        "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info('{table}') ORDER BY cid"
    );
    connection
        .prepare(&sql)
        .expect("schema query must prepare")
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("schema query must run")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("schema rows must decode")
}

fn column(name: &str, data_type: &str, not_null: i64, default: Option<&str>, pk: i64) -> Column {
    (
        name.into(),
        data_type.into(),
        not_null,
        default.map(String::from),
        pk,
    )
}

#[test]
fn migration_v1_columns_match_the_credential_free_schema() {
    // Given: v1 migration を適用したデータベース
    let (_temp, connection) = open_schema();
    let expected = [
        (
            "sessions",
            vec![
                column("id", "TEXT", 0, None, 1),
                column("parent_id", "TEXT", 0, None, 0),
                column("status", "TEXT", 1, None, 0),
                column("failure_reason", "TEXT", 0, None, 0),
                column("delegated_to", "TEXT", 0, None, 0),
                column("total_event_bytes", "INTEGER", 1, Some("0"), 0),
                column("created_at_ns", "INTEGER", 1, None, 0),
                column("updated_at_ns", "INTEGER", 1, None, 0),
            ],
        ),
        (
            "tasks",
            vec![
                column("id", "TEXT", 0, None, 1),
                column("session_id", "TEXT", 0, None, 0),
                column("status", "TEXT", 1, None, 0),
                column("created_at_ns", "INTEGER", 1, None, 0),
                column("updated_at_ns", "INTEGER", 1, None, 0),
            ],
        ),
        (
            "messages",
            vec![
                column("id", "TEXT", 0, None, 1),
                column("session_id", "TEXT", 1, None, 0),
                column("role", "TEXT", 1, None, 0),
                column("content", "TEXT", 1, None, 0),
                column("reasoning", "TEXT", 0, None, 0),
                column("created_at_ns", "INTEGER", 1, None, 0),
                column("updated_at_ns", "INTEGER", 1, None, 0),
            ],
        ),
        (
            "agent_runs",
            vec![
                column("id", "TEXT", 0, None, 1),
                column("session_id", "TEXT", 1, None, 0),
                column("provider", "TEXT", 1, None, 0),
                column("model", "TEXT", 1, None, 0),
                column("status", "TEXT", 1, None, 0),
                column("started_at_ns", "INTEGER", 1, None, 0),
                column("finished_at_ns", "INTEGER", 0, None, 0),
            ],
        ),
        (
            "events",
            vec![
                column("id", "INTEGER", 0, None, 1),
                column("session_id", "TEXT", 0, None, 0),
                column("schema_version", "INTEGER", 1, None, 0),
                column("monotonic_ns", "INTEGER", 1, None, 0),
                column("wall_clock_ns", "INTEGER", 1, None, 0),
                column("kind", "TEXT", 1, None, 0),
                column("payload", "TEXT", 1, None, 0),
            ],
        ),
        (
            "downsampled_metrics",
            vec![
                column("window_start", "INTEGER", 1, None, 1),
                column("provider", "TEXT", 1, None, 2),
                column("model", "TEXT", 1, None, 3),
                column("input_tokens", "INTEGER", 1, Some("0"), 0),
                column("output_tokens", "INTEGER", 1, Some("0"), 0),
                column("cache_read_tokens", "INTEGER", 1, Some("0"), 0),
                column("cache_write_tokens", "INTEGER", 1, Some("0"), 0),
                column("cache_hits", "INTEGER", 1, Some("0"), 0),
                column("cache_misses", "INTEGER", 1, Some("0"), 0),
                column("request_count", "INTEGER", 1, Some("0"), 0),
            ],
        ),
    ];

    // When: 全テーブルの column metadata を取得する
    let actual = TABLES.map(|table| (table, columns(&connection, table)));

    // Then: DDL から転記した完全な snapshot と一致する
    assert_eq!(actual, expected);
}

#[test]
fn migration_v1_column_names_reject_credential_terms() {
    // Given: credential を示す部分一致語と完全一致語
    let (_temp, connection) = open_schema();
    let substrings = [
        "secret",
        "password",
        "credential",
        "api_key",
        "apikey",
        "bearer",
        "auth_token",
    ];
    let exact = ["token", "key", "authorization"];

    // When: 全テーブルの column 名を小文字化する
    let names = TABLES
        .into_iter()
        .flat_map(|table| columns(&connection, table))
        .map(|column| column.0.to_ascii_lowercase())
        .collect::<Vec<_>>();

    // Then: denylist の部分一致・完全一致が一件もない
    assert!(
        names
            .iter()
            .all(|name| !substrings.iter().any(|term| name.contains(term)))
    );
    assert!(names.iter().all(|name| !exact.contains(&name.as_str())));
}
