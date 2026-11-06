//! Upgrade a populated v49 database: the snapshots keep their facts, and the new title
//! column starts empty.
use super::apply_up_to;
use rusqlite::Connection;

#[test]
fn the_title_column_is_added_without_touching_the_stored_facts() {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    apply_up_to(&conn, 49).unwrap();
    conn.execute_batch(
        "INSERT INTO pull_request_snapshots(provider, host, repository, number, state,
           base_branch, head_branch, head_sha, review_decision, fetched_at_ns, success_seq, fetch_seq)
         VALUES ('github', 'github.com', 'o/r', 1, 'open', 'main', 'feat', 'a', 'none', 5, 1, 1);",
    )
    .unwrap();

    apply_up_to(&conn, 50).unwrap();

    let (state, title): (String, Option<String>) = conn
        .query_row(
            "SELECT state, title FROM pull_request_snapshots WHERE number = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), title), ("open", None));
    conn.execute(
        "UPDATE pull_request_snapshots SET title = 'T' WHERE number = 1",
        [],
    )
    .unwrap();
}
