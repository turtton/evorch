//! Regression coverage for pre-stop v8/v9 databases and the edited v8 schema.

use super::*;
use rusqlite::{params, types::Value};

// Captured verbatim with:
// git show 0f8940ac61e3e79006477eef5658a8d249535bdc:crates/storage/src/migrations/v8.sql
const OLD_V8: &str = include_str!("v8_before_stopped.sql");
const V10: &str = include_str!("../../src/migrations/v10.sql");
const OLD_STATUSES: [&str; 8] = [
    "pending",
    "queued",
    "running",
    "blocked",
    "retrying",
    "completed",
    "cancelled",
    "failed",
];

fn legacy_database(path: &Path, version: u32, already_allows_stopped: bool) {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    for migration in include_str!("../../src/migrations/sql.rs")
        .split("r#\"")
        .skip(1)
    {
        conn.execute_batch(migration.split("\"#;").next().unwrap())
            .unwrap();
    }
    for migration in [
        include_str!("../../src/migrations/v3.sql"),
        include_str!("../../src/migrations/v4.sql"),
        include_str!("../../src/migrations/v5.sql"),
        include_str!("../../src/migrations/v6.sql"),
        include_str!("../../src/migrations/v7.sql"),
        if already_allows_stopped {
            include_str!("../../src/migrations/v8.sql")
        } else {
            OLD_V8
        },
    ] {
        conn.execute_batch(migration).unwrap();
    }
    if version == 9 {
        // v9 is unchanged from the same commit as OLD_V8.
        conn.execute_batch(include_str!("../../src/migrations/v9.sql"))
            .unwrap();
    }
    conn.pragma_update(None, "user_version", version).unwrap();
    populate(&conn, already_allows_stopped);
    if !already_allows_stopped {
        assert_check_error(insert_status(&conn, "old-stopped", "stopped").unwrap_err());
    }
}

fn insert_status(conn: &Connection, id: &str, status: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO tasks(id,status,created_at_ns,updated_at_ns) VALUES(?1,?2,10,20)",
        params![id, status],
    )
}

fn populate(conn: &Connection, include_stopped: bool) {
    conn.execute_batch(
        "INSERT INTO sessions(id,status,created_at_ns,updated_at_ns) VALUES('s','running',1,2)",
    )
    .unwrap();
    for status in OLD_STATUSES {
        conn.execute(
            "INSERT INTO tasks VALUES(?1,'s',?1,10,20,'parent','input','progress',
             'artifact','reason','cursor',7,30)",
            [status],
        )
        .unwrap();
    }
    // Also exercise NULL fields and the attempts default.
    insert_status(conn, "minimal", "pending").unwrap();
    if include_stopped {
        insert_status(conn, "stopped", "stopped").unwrap();
    }
    conn.execute_batch(
        "INSERT INTO task_links VALUES('running','blocked'),('completed','pending');",
    )
    .unwrap();
}

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut statement = conn.prepare(sql).unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| (0..columns).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn assert_check_error(error: rusqlite::Error) {
    assert!(
        matches!(&error, rusqlite::Error::SqliteFailure(code, Some(message))
            if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
                && message.contains("CHECK constraint failed")),
        "expected CHECK failure, got {error}"
    );
}

fn verify_constraints(conn: &Connection) {
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(rows(conn, "PRAGMA foreign_key_check").is_empty());
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let indices = schema_objects(conn, "index");
    assert!(indices.contains("idx_tasks_session_id"));
    assert!(indices.contains("idx_task_links_blocked"));
    insert_status(conn, "new-stopped", "stopped").unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT attempts FROM tasks WHERE id='new-stopped'",
            [],
            |row| { row.get::<_, i64>(0) }
        )
        .unwrap(),
        0
    );
    assert_check_error(insert_status(conn, "invalid", "not-a-status").unwrap_err());
    assert_check_error(
        conn.execute("INSERT INTO task_links VALUES('running','running')", [])
            .unwrap_err(),
    );
    for sql in [
        "INSERT INTO task_links VALUES('missing','running')",
        "DELETE FROM tasks WHERE id='running'",
        "UPDATE tasks SET session_id='missing' WHERE id='minimal'",
    ] {
        assert!(matches!(conn.execute(sql, []).unwrap_err(),
            rusqlite::Error::SqliteFailure(code, _)
                if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY));
    }
}

fn upgrade(version: u32, already_allows_stopped: bool) {
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    legacy_database(&path, version, already_allows_stopped);
    let conn = Connection::open(&path).unwrap();
    let before_tasks = rows(&conn, "SELECT * FROM tasks ORDER BY id");
    let before_links = rows(
        &conn,
        "SELECT * FROM task_links ORDER BY blocker_id, blocked_id",
    );
    let before_sessions = rows(&conn, "SELECT * FROM sessions ORDER BY id");
    drop(conn);

    let db = Database::open(&config_for(&path)).unwrap();
    assert_eq!(db.pragma_i64("user_version").unwrap(), 10);
    assert_eq!(db.pragma_i64("foreign_keys").unwrap(), 1);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM tasks ORDER BY id"), before_tasks);
    assert_eq!(
        rows(
            &conn,
            "SELECT * FROM task_links ORDER BY blocker_id, blocked_id"
        ),
        before_links
    );
    assert_eq!(
        rows(&conn, "SELECT * FROM sessions ORDER BY id"),
        before_sessions
    );
    verify_constraints(&conn);

    // A normal reopen is guarded by user_version and does not rebuild anything.
    let schema_version = db.pragma_i64("schema_version").unwrap();
    drop(db);
    let reopened = Database::open(&config_for(&path)).unwrap();
    assert_eq!(
        reopened.pragma_i64("schema_version").unwrap(),
        schema_version
    );
    drop(reopened);

    // The SQL itself can also be safely replayed, including persisted stopped rows.
    let before_replay = rows(&conn, "SELECT * FROM tasks ORDER BY id");
    let tx = conn.unchecked_transaction().unwrap();
    tx.execute_batch(V10).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        rows(&conn, "SELECT * FROM tasks ORDER BY id"),
        before_replay
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT * FROM task_links ORDER BY blocker_id, blocked_id"
        ),
        before_links
    );
    assert!(rows(&conn, "PRAGMA foreign_key_check").is_empty());
}

#[test]
fn old_v8_upgrade_preserves_all_columns_links_and_constraints() {
    upgrade(8, false);
}

#[test]
fn old_v9_upgrade_preserves_all_columns_links_and_constraints() {
    upgrade(9, false);
}

#[test]
fn edited_v8_at_version_9_preserves_existing_stopped_tasks() {
    upgrade(9, true);
}

#[test]
fn fresh_open_accepts_stopped_and_rejects_invalid_status() {
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    let db = Database::open(&config_for(&path)).unwrap();
    assert_eq!(db.pragma_i64("user_version").unwrap(), 10);
    let conn = Connection::open(&path).unwrap();
    populate(&conn, true);
    verify_constraints(&conn);
    assert_eq!(rows(&conn, "SELECT * FROM tasks").len(), 11);
}

#[test]
fn failed_rebuild_rolls_back_schema_data_and_version() {
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    legacy_database(&path, 9, false);
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    insert_status(&conn, "corrupt", "invalid").unwrap();
    conn.pragma_update(None, "ignore_check_constraints", false)
        .unwrap();
    let before = rows(&conn, "SELECT * FROM tasks ORDER BY id");
    let schema = rows(&conn, "SELECT * FROM sqlite_master ORDER BY name");
    assert!(matches!(
        Database::open(&config_for(&path)),
        Err(StorageError::Migration { version: 10, .. })
    ));
    assert_eq!(rows(&conn, "SELECT * FROM tasks ORDER BY id"), before);
    assert_eq!(
        rows(&conn, "SELECT * FROM sqlite_master ORDER BY name"),
        schema
    );
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        9
    );
}
