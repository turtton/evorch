//! The renderer metadata index must seek inside a thread, before pagination.
//! SQL statement counters, not elapsed time, bound the work of each page.
use rusqlite::{Connection, StatementStatus, params};
use storage::{Database, StorageConfig};

const PAGE: &str = "SELECT artifact_id FROM artifact_refs \
    WHERE thread_id = ?1 AND artifact_id > ?2 ORDER BY artifact_id LIMIT 2";

fn assert_seek(sql: &Connection, cursor: &str) -> (Vec<String>, i32) {
    let plan: Vec<String> = sql
        .prepare(&format!("EXPLAIN QUERY PLAN {PAGE}"))
        .unwrap()
        .query_map(params!["owned", cursor], |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        plan.iter().any(|step| {
            step.contains("SEARCH")
                && step.contains("thread_id=?")
                && step.contains("artifact_id>?")
        }),
        "a page must seek by both thread and cursor: {plan:?}"
    );
    let mut statement = sql.prepare(PAGE).unwrap();
    let items = statement
        .query_map(params!["owned", cursor], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
    assert_eq!(statement.get_status(StatementStatus::Sort), 0);
    (items, statement.get_status(StatementStatus::VmStep))
}

fn seed(sql: &Connection) {
    sql.execute_batch(
        "INSERT INTO artifact_refs VALUES
         ('a', 'same-blob', 'owned', 'run-a', 1),
         ('b', 'same-blob', 'other', 'run-b', 2),
         ('c', 'same-blob', 'owned', NULL, 3),
         ('d', 'same-blob', NULL, NULL, 4),
         ('e', 'other-blob', 'owned', 'run-e', 5);",
    )
    .unwrap();
}

#[test]
fn upgrading_v68_preserves_references_and_adds_a_thread_cursor_seek() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("upgrade.db"),
        ..Default::default()
    };
    let before = Database::open(&config).unwrap();
    let sql = Connection::open(&config.db_path).unwrap();
    seed(&sql);
    let expected = before.artifact_refs_after(None, 10).unwrap();
    drop(before);
    // Reconstruct v68, as the other historical migration fixtures do.
    sql.execute_batch(concat!(
        include_str!("../undo/undo_v73.sql"),
        include_str!("../undo/undo_v72.sql"),
        include_str!("../undo/undo_v71.sql"),
        include_str!("../undo/undo_v70.sql"),
        include_str!("../undo/undo_v69.sql")
    ))
    .unwrap();
    sql.pragma_update(None, "user_version", 68).unwrap();
    drop(sql);
    let upgraded = Database::open(&config).unwrap();
    assert_eq!(upgraded.artifact_refs_after(None, 10).unwrap(), expected);
    drop(upgraded);
    // A second open must not rerun CREATE INDEX.
    drop(Database::open(&config).unwrap());
    let sql = Connection::open(&config.db_path).unwrap();
    let (first, _) = assert_seek(&sql, "");
    assert_eq!(first, ["a", "c"]);
    let (next, _) = assert_seek(&sql, "c");
    assert_eq!(next, ["e"]);
    assert!(assert_seek(&sql, "e").0.is_empty());
}

#[test]
fn fresh_indexed_pages_do_not_walk_unrelated_threads() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("fresh.db"),
        ..Default::default()
    };
    drop(Database::open(&config).unwrap());
    let sql = Connection::open(&config.db_path).unwrap();
    seed(&sql);
    let first = assert_seek(&sql, "");
    let next = assert_seek(&sql, "c");
    // Insert many other-thread rows both before and after the page's cursor.
    sql.execute_batch(
        "WITH RECURSIVE n(x) AS (
             VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 10000
         )
         INSERT INTO artifact_refs
         SELECT printf('b-%05d', x), 'shared', 'other', NULL, x FROM n;
         WITH RECURSIVE n(x) AS (
             VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 10000
         )
         INSERT INTO artifact_refs
         SELECT printf('d-%05d', x), 'shared', 'other', NULL, x FROM n;",
    )
    .unwrap();
    // Both page boundaries and VM steps stay unchanged as foreign rows grow.
    assert_eq!(assert_seek(&sql, ""), first);
    assert_eq!(assert_seek(&sql, "c"), next);
}
