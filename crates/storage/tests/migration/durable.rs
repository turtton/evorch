use super::*;

#[test]
fn v8_upgrade_preserves_task_links_and_defaults() {
    // Given: the complete v7 schema with linked tasks and a session.
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    let connection = Connection::open(&path).unwrap();
    for migration in include_str!("../../src/migrations/sql.rs")
        .split("r#\"")
        .skip(1)
    {
        connection
            .execute_batch(migration.split("\"#;").next().unwrap())
            .unwrap();
    }
    for migration in [
        include_str!("../../src/migrations/v3.sql"),
        include_str!("../../src/migrations/v4.sql"),
        include_str!("../../src/migrations/v5.sql"),
        include_str!("../../src/migrations/v6.sql"),
        include_str!("../../src/migrations/v7.sql"),
    ] {
        connection.execute_batch(migration).unwrap();
    }
    connection.pragma_update(None, "user_version", 7).unwrap();
    connection.execute_batch("INSERT INTO sessions(id,status,created_at_ns,updated_at_ns) VALUES('s','running',1,2); INSERT INTO tasks VALUES('a','s','running',10,20),('b','s','blocked',30,40); INSERT INTO task_links VALUES('a','b');").unwrap();
    drop(connection);
    // When: opening upgrades with foreign keys enabled.
    let db = Database::open(&config_for(&path)).unwrap();
    // Then: records, links and constraints survive, and new fields have defaults.
    let task = db.task("a").unwrap().unwrap();
    assert_eq!(task.session_id.as_deref(), Some("s"));
    assert_eq!(
        task.created_at,
        std::time::UNIX_EPOCH + std::time::Duration::from_nanos(10)
    );
    assert_eq!(
        task.updated_at,
        std::time::UNIX_EPOCH + std::time::Duration::from_nanos(20)
    );
    assert_eq!(task.attempts, 0);
    assert_eq!(
        (
            task.parent_run_id,
            task.input,
            task.progress,
            task.last_artifact,
            task.failure_reason,
            task.resume_cursor,
            task.heartbeat_at
        ),
        (None, None, None, None, None, None, None)
    );
    assert_eq!(db.task_dependencies("b").unwrap().blocked_by, vec!["a"]);
    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get::<_, u32>(0)
            })
            .unwrap(),
        0
    );
    assert!(
        connection
            .execute("DELETE FROM tasks WHERE id='a'", [])
            .is_err()
    );
}
