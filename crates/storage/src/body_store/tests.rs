use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::json;

use super::test_owner::{
    OWNER, OWNERS, count, database, immediate, load, put, store, store_b, text,
};
use super::*;
use crate::Database;
use crate::body_ref::BodyToken;
use crate::error::BodyUnresolvedReason;

fn unresolved_reason(error: StorageError) -> BodyUnresolvedReason {
    match error {
        StorageError::BodyUnresolved { reason, .. } => reason,
        other => panic!("expected BodyUnresolved, got {other:?}"),
    }
}

fn blob_count(conn: &Connection) -> i64 {
    count(conn, "SELECT COUNT(*) FROM content_blobs")
}

fn ref_count(conn: &Connection) -> i64 {
    count(conn, "SELECT COUNT(*) FROM content_blob_refs_test_owner")
}

#[test]
fn a_stored_owner_row_resolves_back_to_the_original_value() {
    // Given: a body with two long strings (one repeated), a short one and an escape string.
    let conn = &database().conn;
    let long = text("a", 5_000);
    let body = json!({
        "m": [long.clone(), "short", { "again": long.clone() }, text("b", 2_048)],
        "escaped": "\u{e000}evorch-body:v1:not-a-token",
    });

    // When: it is stored through the owner entry point.
    store(conn, "o1", &body).unwrap();

    // Then: the row holds tokens only, three distinct blobs back it, the repeated string
    // has one reference, and resolving restores the exact value.
    let stored: String = conn
        .query_row("SELECT body FROM test_owner WHERE id = 'o1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(!stored.contains(&long));
    assert_eq!(blob_count(conn), 3);
    assert_eq!(ref_count(conn), 3);
    assert_eq!(load(conn, "o1").unwrap(), body);
}

#[test]
fn identical_text_is_stored_once_and_reused_without_writing() {
    // Given: a blob already stored.
    let conn = &database().conn;
    let body = text("dup", 4_000);
    let tx = immediate(conn);
    let first = put_blob(&tx, &body).unwrap();
    assert!(first.created);

    // When: the same text is stored again in the same and in a later transaction.
    let changes = tx.total_changes();
    let again = put_blob(&tx, &body).unwrap();
    tx.commit().unwrap();
    let tx = immediate(conn);
    let changes_later = tx.total_changes();
    let later = put_blob(&tx, &body).unwrap();

    // Then: the same row is returned, nothing is created and no row is written.
    assert_eq!((again.id, again.created), (first.id, false));
    assert_eq!((later.id, later.created), (first.id, false));
    assert_eq!(tx.total_changes(), changes_later);
    assert_eq!(changes, changes_later);
    assert_eq!(blob_count(&tx), 1);
}

#[test]
fn compressible_text_is_stored_compressed_and_other_text_as_it_is() {
    let conn = &database().conn;
    let repetitive = "abc".repeat(3_000);
    // A 4-byte text cannot get smaller as a zstd frame.
    let tiny = "\u{e000}x";
    for text in [repetitive.as_str(), tiny] {
        let tx = immediate(conn);
        let stored = put_blob(&tx, text).unwrap();
        let loaded = load_blob(&tx, &stored.token, "test").unwrap();
        tx.commit().unwrap();
        assert_eq!(loaded, text);
    }
    let codecs: Vec<(i64, i64, i64)> = conn
        .prepare("SELECT codec, byte_len, OCTET_LENGTH(data) FROM content_blobs ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(codecs[0].0, CODEC_ZSTD);
    assert!(codecs[0].2 < codecs[0].1 / 10);
    assert_eq!(codecs[1], (CODEC_PLAIN, 4, 4));
}

#[test]
fn writes_outside_a_write_transaction_are_refused() {
    let conn = &database().conn;
    // No transaction at all.
    assert!(put_blob(conn, "x").is_err());
    // A deferred transaction that has not written holds no write lock.
    conn.execute_batch("BEGIN").unwrap();
    assert!(put_blob(conn, "x").is_err());
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(blob_count(conn), 0);
    // The owner entry point checks the same.
    let mut containers = [json!("x")];
    let error = put_owner(
        conn,
        &OWNER,
        &[SqlValue::Text("o".into())],
        &mut containers,
        |_, _| Ok(()),
    )
    .unwrap_err();
    assert!(matches!(error, StorageError::Serialization(_)));
}

#[test]
fn updating_an_owner_row_moves_its_references_and_queues_the_dropped_ones() {
    // Given: a row naming blobs A and B.
    let conn = &database().conn;
    let (a, b, c) = (text("a", 3_000), text("b", 3_000), text("c", 3_000));
    store(conn, "o", &json!([a.clone(), b.clone()])).unwrap();
    let id_of = |body: &str| -> i64 {
        conn.query_row(
            "SELECT id FROM content_blobs WHERE hash = ?1",
            [&BodyToken::of(body).hash[..]],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(count(conn, "SELECT COUNT(*) FROM content_blob_gc_queue"), 0);

    // When: it is rewritten to name B and C.
    store(conn, "o", &json!([b.clone(), c.clone()])).unwrap();

    // Then: the references are exactly {B, C} (none missing, none left over), and only
    // the dropped blob A was queued.
    let referenced: Vec<i64> = conn
        .prepare("SELECT blob_id FROM content_blob_refs_test_owner ORDER BY blob_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut expected = vec![id_of(&b), id_of(&c)];
    expected.sort_unstable();
    assert_eq!(referenced, expected);
    let queued: Vec<i64> = conn
        .prepare("SELECT blob_id FROM content_blob_gc_queue")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(queued, vec![id_of(&a)]);
    assert_eq!(load(conn, "o").unwrap(), json!([b, c]));
}

#[test]
fn deleting_an_owner_row_cascades_its_references_and_queues_every_blob() {
    // Given: two owners sharing one blob and each holding one of its own.
    let conn = &database().conn;
    let shared = text("shared", 3_000);
    store(conn, "o1", &json!([shared.clone(), text("only1", 3_000)])).unwrap();
    store_b(conn, 7, &json!([shared, text("only2", 3_000)])).unwrap();
    assert_eq!(blob_count(conn), 3);

    // When: the first owner row is deleted.
    conn.execute("DELETE FROM test_owner WHERE id = 'o1'", [])
        .unwrap();

    // Then: its references went with it (the foreign key cascade fired the trigger),
    // both its blobs are queued unconditionally, and the other owner's are untouched.
    assert_eq!(ref_count(conn), 0);
    assert_eq!(
        count(conn, "SELECT COUNT(*) FROM content_blob_refs_test_owner_b"),
        2
    );
    assert_eq!(count(conn, "SELECT COUNT(*) FROM content_blob_gc_queue"), 2);
}

#[test]
fn a_failure_anywhere_in_the_entry_point_leaves_nothing_behind() {
    let conn = &database().conn;
    let body = json!([text("f", 3_000)]);

    // The owner row write fails after the blob was inserted.
    let tx = immediate(conn);
    let mut containers = [body.clone()];
    let error = put_owner(
        &tx,
        &OWNER,
        &[SqlValue::Text("x".into())],
        &mut containers,
        |_, _| -> Result<(), StorageError> { Err(StorageError::WriterClosed) },
    )
    .unwrap_err();
    assert_eq!(error, StorageError::WriterClosed);
    // The caller commits anyway: the savepoint already undid the call.
    tx.commit().unwrap();
    assert_eq!(blob_count(conn), 0);

    // The reference insert fails (injected) after blob and owner row were written.
    conn.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON content_blob_refs_test_owner
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let tx = immediate(conn);
    assert!(put(&tx, "y", None, &body).is_err());
    tx.commit().unwrap();
    assert_eq!(blob_count(conn), 0);
    assert_eq!(count(conn, "SELECT COUNT(*) FROM test_owner"), 0);
    conn.execute_batch("DROP TRIGGER inject").unwrap();
    store(conn, "y", &body).unwrap();
    assert_eq!(blob_count(conn), 1);
}

#[test]
fn a_hash_stored_with_another_length_aborts_the_whole_write() {
    // Given: a row with the hash of a text but a different length (corruption).
    let conn = &database().conn;
    let body = text("c", 3_000);
    let token = BodyToken::of(&body);
    conn.execute(
        "INSERT INTO content_blobs(hash, byte_len, codec, data) VALUES (?1, 7, 0, 'garbage')",
        [&token.hash[..]],
    )
    .unwrap();

    // When: that text is stored for an owner (together with another long string).
    let error = store(conn, "o", &json!([text("other", 3_000), body])).unwrap_err();

    // Then: it is a conflict, and neither the other blob, the owner row nor any
    // reference was kept.
    assert_eq!(
        unresolved_reason(error),
        BodyUnresolvedReason::HashLengthConflict {
            stored: 7,
            new: 3_000
        }
    );
    assert_eq!(blob_count(conn), 1);
    assert_eq!(count(conn, "SELECT COUNT(*) FROM test_owner"), 0);
    assert_eq!(ref_count(conn), 0);
}

#[test]
fn unresolvable_tokens_are_hard_errors_with_the_hash_when_known() {
    let conn = &database().conn;
    let body = text("u", 3_000);
    store(conn, "o", &json!([body.clone()])).unwrap();
    let token = BodyToken::of(&body);
    let by_hash = |sql: &str| {
        conn.execute(sql, [&token.hash[..]]).unwrap();
    };

    // A length that differs from the token's.
    by_hash("UPDATE content_blobs SET byte_len = byte_len + 1 WHERE hash = ?1");
    let error = load(conn, "o").unwrap_err();
    assert_eq!(
        unresolved_reason(error),
        BodyUnresolvedReason::LengthMismatch {
            expected: 3_000,
            actual: 3_001
        }
    );
    by_hash("UPDATE content_blobs SET byte_len = byte_len - 1 WHERE hash = ?1");

    // Compressed data that is not a valid frame.
    by_hash("UPDATE content_blobs SET data = x'0102030405' WHERE hash = ?1");
    let (codec, ..) = blob_row(conn, &token);
    assert_eq!(codec, CODEC_ZSTD);
    let error = load(conn, "o").unwrap_err();
    assert_eq!(
        unresolved_reason(error),
        BodyUnresolvedReason::DecompressFailed
    );

    // Plain data of the right length that is not UTF-8.
    by_hash(
        "UPDATE content_blobs SET codec = 0, data = CAST(x'ff' || zeroblob(2999) AS BLOB) \
         WHERE hash = ?1",
    );
    let error = load(conn, "o").unwrap_err();
    assert_eq!(unresolved_reason(error), BodyUnresolvedReason::InvalidUtf8);

    // A value of the wrong SQL type in the data column.
    by_hash("UPDATE content_blobs SET data = 5 WHERE hash = ?1");
    let error = load(conn, "o").unwrap_err();
    assert_eq!(unresolved_reason(error), BodyUnresolvedReason::MalformedRow);

    // The blob is gone (foreign keys off to make the corruption possible).
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    by_hash("DELETE FROM content_blobs WHERE hash = ?1");
    let error = load(conn, "o").unwrap_err();
    let StorageError::BodyUnresolved {
        owner,
        hash,
        reason,
    } = error
    else {
        panic!("expected BodyUnresolved");
    };
    assert_eq!(reason, BodyUnresolvedReason::BlobMissing);
    assert_eq!(hash, Some(token.hash_hex()));
    assert!(owner.contains("id=o"));

    // A token that is not one.
    conn.execute(
        "UPDATE test_owner SET body = ?1",
        ["[\"\u{e000}evorch-body:v7:oops\"]"],
    )
    .unwrap();
    let error = load(conn, "o").unwrap_err();
    assert_eq!(
        unresolved_reason(error),
        BodyUnresolvedReason::UnknownTokenVersion
    );
}

fn blob_row(conn: &Connection, token: &BodyToken) -> (i64, i64, Vec<u8>) {
    conn.query_row(
        "SELECT codec, byte_len, data FROM content_blobs WHERE hash = ?1",
        [&token.hash[..]],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

#[test]
fn decoding_checks_codec_declared_length_and_frame_size() {
    let fail = |codec, byte_len, data: &[u8]| {
        unresolved_reason(decode_blob(byte_len, codec, data, "t", "h").unwrap_err())
    };
    assert_eq!(fail(5, 3, b"abc"), BodyUnresolvedReason::UnknownCodec(5));
    assert_eq!(
        fail(CODEC_PLAIN, 4, b"abc"),
        BodyUnresolvedReason::LengthMismatch {
            expected: 4,
            actual: 3
        }
    );
    assert_eq!(
        fail(CODEC_PLAIN, -1, b""),
        BodyUnresolvedReason::DeclaredLengthTooLarge { declared: u64::MAX }
    );
    assert_eq!(
        fail(CODEC_PLAIN, MAX_BLOB_BYTES as i64 + 1, b""),
        BodyUnresolvedReason::DeclaredLengthTooLarge {
            declared: MAX_BLOB_BYTES + 1
        }
    );
    // A frame whose content size differs from the declaration is refused before
    // anything is allocated for it.
    let frame = zstd::bulk::compress("abc".repeat(1_000).as_bytes(), 3).unwrap();
    assert_eq!(
        fail(CODEC_ZSTD, 10, &frame),
        BodyUnresolvedReason::LengthMismatch {
            expected: 10,
            actual: 3_000
        }
    );
    assert_eq!(
        decode_blob(3_000, CODEC_ZSTD, &frame, "t", "h").unwrap(),
        "abc".repeat(1_000)
    );
}

#[test]
fn verification_catches_a_text_that_does_not_match_the_hash() {
    let token = BodyToken::of("the real text");
    assert!(verify_decoded("the real text", &token.hash, "t").is_ok());
    let error = verify_decoded("another text", &token.hash, "t").unwrap_err();
    assert_eq!(unresolved_reason(error), BodyUnresolvedReason::HashMismatch);
}

#[test]
fn reference_tables_follow_the_template_and_a_probe_uses_the_blob_index() {
    let conn = &database().conn;
    for owner in OWNERS {
        let plan: Vec<String> = conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT 1 FROM {} WHERE blob_id = 1",
                owner.ref_table
            ))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().all(|step| !step.contains("SCAN")),
            "{}: {plan:?}",
            owner.ref_table
        );
    }
    // The generated trigger queues unconditionally: no other-owner test inside it.
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1",
            [OWNER.ref_table],
            |row| row.get(0),
        )
        .unwrap();
    assert!(sql.contains("INSERT OR IGNORE INTO content_blob_gc_queue"));
    assert!(!sql.contains("NOT EXISTS"));
}

#[test]
fn production_registry_matches_the_reference_tables_of_a_fresh_schema() {
    // Given: a database with only the production schema.
    let database = Database::open_in_memory().unwrap();
    let tables = crate::repo::content_blobs::ref_tables(&database.conn).unwrap();

    // Then: its reference tables are exactly the registered production owners', so a
    // table cannot be added without registering it, nor registered without a table.
    let mut registered: Vec<&str> = PRODUCTION_OWNERS
        .iter()
        .map(|owner| owner.ref_table)
        .collect();
    registered.sort_unstable();
    assert_eq!(tables, registered);
    for owner in PRODUCTION_OWNERS {
        assert!(owner.ref_table.starts_with(REF_TABLE_PREFIX));
    }
}

#[test]
fn the_blob_foreign_key_refuses_a_reference_to_a_missing_blob() {
    let conn = &database().conn;
    let on: i64 = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .unwrap();
    assert_eq!(on, 1);
    store(conn, "k", &json!("short")).unwrap();
    let error = conn
        .execute(
            "INSERT INTO content_blob_refs_test_owner(owner_id, blob_id) VALUES ('k', 999)",
            [],
        )
        .unwrap_err();
    assert!(error.to_string().contains("FOREIGN KEY"));
}
