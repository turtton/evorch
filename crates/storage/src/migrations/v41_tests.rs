//! Migration v41 on a database that already holds threads: the rebuild of `threads`
//! keeps every row, index and fence, and only adds the lane vocabulary.

use rusqlite::Connection;

use super::apply_up_to;

fn names(conn: &Connection, kind: &str, table: &str) -> Vec<String> {
    let mut statement = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = ?1 AND tbl_name = ?2 \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    statement
        .query_map([kind, table], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn thread_rows(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare(
            "SELECT thread_id || '|' || COALESCE(project_root, '-') || '|' || COALESCE(parent_thread_id, '-') \
             || '|' || COALESCE(lineage_kind, '-') || '|' || archived || '|' || pinned || '|' \
             || COALESCE(archive_revision, -1) || '|' || COALESCE(pin_revision, -1) || '|' \
             || COALESCE(archived_at_ns, -1) || '|' || updated_at_ns || '|' || source \
             FROM threads ORDER BY thread_id",
        )
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn at_v40() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    apply_up_to(&conn, 40).unwrap();
    conn
}

#[test]
fn existing_threads_survive_the_rebuild_with_their_indexes_and_fences() {
    // Given: a version 40 database with differently shaped threads and a tombstone.
    let conn = at_v40();
    conn.execute_batch(
        "INSERT INTO threads(thread_id, project_root, parent_thread_id, lineage_kind, archived, \
            superseded, created_at_ns, updated_at_ns, source, archived_at_ns, pinned, \
            archive_revision, pin_revision, root_run_id, escalation_source_run_id) VALUES
            ('root', '/p', NULL, NULL, 0, 0, 1, 5, 'host', NULL, 1, NULL, 7, 'r0', NULL),
            ('forked', '/p', 'root', 'fork', 1, 1, 2, 6, 'runtime', 99, 0, 3, NULL, NULL, NULL),
            ('escalated', '/q', 'root', 'escalation', 0, 0, 3, 7, 'backfill', NULL, 0, NULL, NULL, 'r1', 'r0');
         INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) VALUES ('gone', 'f', 1);",
    )
    .unwrap();
    let rows = thread_rows(&conn);
    let indexes = names(&conn, "index", "threads");
    let triggers = names(&conn, "trigger", "threads");

    // When: migration v41 runs.
    apply_up_to(&conn, 41).unwrap();

    // Then: every row is the same, every index is back, and the two fences still hold.
    assert_eq!(thread_rows(&conn), rows);
    assert_eq!(names(&conn, "index", "threads"), indexes);
    let after = names(&conn, "trigger", "threads");
    assert!(
        triggers.iter().all(|trigger| after.contains(trigger)),
        "{after:?}"
    );
    assert!(after.contains(&"threads_lane_lineage_sticky".to_owned()));
    for sql in [
        "INSERT INTO threads(thread_id, created_at_ns, updated_at_ns, source) VALUES ('gone', 1, 1, 'host')",
        "INSERT INTO threads(thread_id, parent_thread_id, created_at_ns, updated_at_ns, source) VALUES ('child', 'gone', 1, 1, 'host')",
        "UPDATE threads SET parent_thread_id = 'gone' WHERE thread_id = 'forked'",
    ] {
        assert!(conn.execute(sql, []).is_err(), "{sql}");
    }

    // And: 'lane' is now a lineage kind, which a stored lane never loses.
    conn.execute(
        "INSERT INTO threads(thread_id, lineage_kind, created_at_ns, updated_at_ns, source) \
         VALUES ('a-lane', 'lane', 1, 1, 'runtime')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "UPDATE threads SET lineage_kind = 'fork' WHERE thread_id = 'a-lane'",
            []
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO threads(thread_id, lineage_kind, created_at_ns, updated_at_ns, source) \
             VALUES ('bad', 'nonsense', 1, 1, 'runtime')",
            []
        )
        .is_err()
    );
    let ledger_revision: i64 = conn
        .query_row("SELECT revision FROM lane_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(ledger_revision, 0);
}
