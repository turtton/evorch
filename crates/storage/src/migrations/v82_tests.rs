//! Migration v82 only adds nullable columns: an old row stays as it was and is not editable.
use super::*;

fn edit_columns(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare(
            "SELECT name FROM pragma_table_info('host_chat_submissions') WHERE name LIKE 'edit_%'",
        )
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn old_rows_keep_their_identity_and_have_no_edit_data() {
    let conn = Connection::open_in_memory().unwrap();
    apply_up_to(&conn, 81).unwrap();
    conn.execute(
        "INSERT INTO host_chat_submissions(submission_id,fingerprint,run_id,provenance_json,accepted_at_ns) \
         VALUES('old','digest','run','{}',7)",
        [],
    )
    .unwrap();

    apply_migrations(&conn).unwrap();

    let (fingerprint, preview, original): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT fingerprint, edit_preview, edit_original_text FROM host_chat_submissions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(fingerprint, "digest");
    assert_eq!((preview, original), (None, None));
    // The words are the last column, so a preview never reads past a large body.
    assert_eq!(edit_columns(&conn).last().unwrap(), "edit_original_text");
}

#[test]
fn undo_removes_the_columns_and_restores_the_version() {
    let conn = Connection::open_in_memory().unwrap();
    apply_migrations(&conn).unwrap();
    assert_eq!(edit_columns(&conn).len(), 5);

    conn.execute_batch(include_str!("../../undo/undo_v82.sql"))
        .unwrap();

    assert!(edit_columns(&conn).is_empty());
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        81
    );
}
