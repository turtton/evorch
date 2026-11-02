use serde_json::json;

use super::*;
use crate::body_store::test_owner::{database, store, text};

#[test]
fn inspect_blob_reports_size_references_and_a_bounded_preview() {
    // Given: a 4,000-byte body (multi-byte characters) referenced by one owner row.
    let database = database();
    let body = "あ".repeat(1_400);
    store(&database.conn, "o1", &json!([body.clone()])).unwrap();
    let hash = crate::body_ref::BodyToken::of(&body).hash_hex();

    // When: it is inspected by a 12-digit prefix.
    let preview = database.inspect_blob(&hash[..12], false).unwrap();
    let full = database.inspect_blob(&hash, true).unwrap();

    // Then: sizes and the referencing row are shown; the preview ends on a character
    // boundary at or below 2 KiB, and the full view returns the whole body.
    assert_eq!(preview["hash"], hash);
    assert_eq!(preview["byte_len"], 4_200);
    assert_eq!(
        preview["referenced_by"][0]["table"],
        "content_blob_refs_test_owner"
    );
    assert_eq!(preview["referenced_by"][0]["owners"][0]["owner_id"], "o1");
    assert_eq!(preview["waiting_for_collection"], false);
    assert_eq!(preview["content"]["truncated"], true);
    assert_eq!(preview["content"]["shown_bytes"], 2_046);
    assert_eq!(preview["content"]["text"].as_str().unwrap().len(), 2_046);
    assert_eq!(full["content"]["truncated"], false);
    assert_eq!(full["content"]["text"], body);
}

#[test]
fn inspect_blob_distinguishes_waiting_blobs_and_rejects_bad_prefixes() {
    let database = database();
    let conn = &database.conn;
    let body = text("w", 3_000);
    store(conn, "o1", &json!([body.clone()])).unwrap();
    conn.execute("DELETE FROM test_owner", []).unwrap();
    let hash = crate::body_ref::BodyToken::of(&body).hash_hex();

    let waiting = database.inspect_blob(&hash[..16], false).unwrap();
    assert_eq!(waiting["waiting_for_collection"], true);
    assert_eq!(waiting["referenced_by"], json!([]));

    for (prefix, expected) in [
        ("abc", "at least 12"),
        ("zzzzzzzzzzzz", "lowercase hex"),
        ("000000000000", "no blob"),
    ] {
        let error = database
            .inspect_blob(prefix, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{prefix}: {error}");
    }
    for last in [1_u8, 2] {
        let mut raw = [0xcd_u8; 32];
        raw[31] = last;
        conn.execute(
            "INSERT INTO content_blobs(hash, byte_len, codec, data) VALUES (?1, 1, 0, 'x')",
            [&raw[..]],
        )
        .unwrap();
    }
    let error = database
        .inspect_blob(&"cd".repeat(10), false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("ambiguous"), "{error}");
}

#[test]
fn a_blob_that_cannot_be_decoded_is_shown_with_the_reason_not_as_a_failure() {
    let database = database();
    let conn = &database.conn;
    let body = text("bad", 3_000);
    store(conn, "o1", &json!([body.clone()])).unwrap();
    conn.execute("UPDATE content_blobs SET data = x'00'", [])
        .unwrap();
    let hash = crate::body_ref::BodyToken::of(&body).hash_hex();
    let shown = database.inspect_blob(&hash, false).unwrap();
    assert!(
        shown["content_error"]
            .as_str()
            .unwrap()
            .contains("decompression")
    );
    assert!(shown.get("content").is_none());
}

#[test]
fn check_bodies_reports_ok_false_when_a_problem_exists() {
    let database = database();
    let conn = &database.conn;
    store(conn, "o1", &json!([text("a", 3_000)])).unwrap();
    // The production registry has no owners yet, so only the global checks run.
    let clean = database
        .inspect_check_bodies(&BodyCheckScope::All, true)
        .unwrap();
    assert_eq!(
        clean["ok"], false,
        "the test owner's tables are unregistered: {clean}"
    );
    assert!(
        clean["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["kind"] == "unregistered_ref_table")
    );
    conn.execute_batch(
        "DROP TABLE content_blob_refs_test_owner; DROP TABLE content_blob_refs_test_owner_b;
         DROP TABLE test_owner; DROP TABLE test_owner_b;
         DELETE FROM content_blobs;",
    )
    .unwrap();
    let ok = database
        .inspect_check_bodies(&BodyCheckScope::All, true)
        .unwrap();
    assert_eq!(ok["ok"], true, "{ok}");
    assert_eq!(ok["problem_count"], 0);
}

#[test]
fn inspection_works_on_a_read_only_connection_and_writes_nothing() {
    // Given: a file database with a blob, opened read-only (which cannot write).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inspect.db");
    let hash = {
        let database = crate::Database::open(&crate::StorageConfig {
            db_path: path.clone(),
            ..Default::default()
        })
        .unwrap();
        crate::body_store::test_owner::install(&database.conn);
        let body = text("ro", 3_000);
        store(&database.conn, "o1", &json!([body.clone()])).unwrap();
        database
            .conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        crate::body_ref::BodyToken::of(&body).hash_hex()
    };
    let readonly = crate::Database::open_read_only(&path).unwrap();
    let before = std::fs::read(&path).unwrap();

    // When: every inspection entry point runs.
    readonly.inspect_blob(&hash, true).unwrap();
    readonly
        .inspect_check_bodies(&BodyCheckScope::All, true)
        .unwrap();
    readonly
        .inspect_check_bodies(&BodyCheckScope::Run("r".into()), false)
        .unwrap();
    let summary = readonly
        .storage_summary(&path, crate::inspect::StorageSummaryOptions::default())
        .unwrap();

    // Then: they succeeded on a connection that cannot write, the file is unchanged, and
    // the summary has its bodies section.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(summary["bodies"]["blobs"], 1);
}
