use serde_json::json;

use super::*;

const FULL: BlobOptions = BlobOptions {
    full_content: true,
    reference_offset: 0,
    reference_limit: DEFAULT_REFERENCES_LISTED,
};
use crate::body_store::test_owner::{database, store, text};

#[test]
fn inspect_blob_reports_size_references_and_a_bounded_preview() {
    // Given: a 4,000-byte body (multi-byte characters) referenced by one owner row.
    let database = database();
    let body = "あ".repeat(1_400);
    store(&database.conn, "o1", &json!([body.clone()])).unwrap();
    let hash = crate::body_ref::BodyToken::of(&body).hash_hex();

    // When: it is inspected by a 12-digit prefix.
    let preview = database
        .inspect_blob(&hash[..12], BlobOptions::default())
        .unwrap();
    let full = database.inspect_blob(&hash, FULL).unwrap();

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

    let waiting = database
        .inspect_blob(&hash[..16], BlobOptions::default())
        .unwrap();
    assert_eq!(waiting["waiting_for_collection"], true);
    assert_eq!(waiting["referenced_by"], json!([]));

    for (prefix, expected) in [
        ("abc", "at least 12"),
        ("zzzzzzzzzzzz", "lowercase hex"),
        ("000000000000", "no blob"),
    ] {
        let error = database
            .inspect_blob(prefix, BlobOptions::default())
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
        .inspect_blob(&"cd".repeat(10), BlobOptions::default())
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
    let shown = database
        .inspect_blob(&hash, BlobOptions::default())
        .unwrap();
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
    readonly.inspect_blob(&hash, FULL).unwrap();
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

#[test]
fn a_widely_shared_blob_lists_its_owners_a_page_at_a_time() {
    // Given: a blob referenced by 25 owner rows.
    let database = database();
    let conn = &database.conn;
    let body = text("shared", 3_000);
    for index in 0..25 {
        store(conn, &format!("o{index:02}"), &json!([body.clone()])).unwrap();
    }
    let hash = crate::body_ref::BodyToken::of(&body).hash_hex();
    let page = |offset, limit| {
        let options = BlobOptions {
            reference_offset: offset,
            reference_limit: limit,
            ..BlobOptions::default()
        };
        let shown = database.inspect_blob(&hash, options).unwrap();
        let table = shown["referenced_by"][0].clone();
        let owners: Vec<String> = table["owners"]
            .as_array()
            .unwrap()
            .iter()
            .map(|owner| owner["owner_id"].as_str().unwrap().to_owned())
            .collect();
        (
            owners,
            table["rows"].as_i64().unwrap(),
            table["next_offset"].clone(),
        )
    };

    // When: the pages are read one after the other.
    let (first, total, next) = page(0, 10);
    let (second, _, next_second) = page(10, 10);
    let (third, _, next_third) = page(20, 10);

    // Then: every owner appears once, in key order, with the total and where to go on.
    assert_eq!(total, 25);
    assert_eq!(
        (next, next_second, next_third),
        (json!(10), json!(20), json!(null))
    );
    let all: Vec<String> = [first, second, third].concat();
    let expected: Vec<String> = (0..25).map(|index| format!("o{index:02}")).collect();
    assert_eq!(all, expected);
    // The default page is 20 rows and says so.
    let (default, _, next) = page(0, DEFAULT_REFERENCES_LISTED);
    assert_eq!((default.len(), next), (20, json!(20)));
}

/// A run of `count` messages, some of them blob-backed.
fn run_record(count: usize) -> crate::RunContextRecord {
    let messages: Vec<serde_json::Value> = (0..count)
        .map(|index| {
            json!({"role": "user", "content": [{"type": "text", "text":
                if index % 2 == 0 { text("m", 3_000 + index) } else { format!("short {index}") }}]})
        })
        .collect();
    let mut record = crate::repo::run_messages::fixtures::record("run-a");
    record.messages_json = serde_json::to_string(&messages).unwrap();
    record
}

#[test]
fn a_save_committed_between_the_reference_check_and_the_run_check_is_not_an_inconsistency() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // One run of the check with a save by another connection injected when the
    // progress handler is called for the `inject_at`-th time (a count of VM
    // instructions, so the point is the same on every run). Returns the report and how
    // many times the handler was called.
    let run = |inject_at: usize| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("check.db");
        let config = crate::StorageConfig {
            db_path: path.clone(),
            ..Default::default()
        };
        let writer = Database::open(&config).unwrap();
        crate::repo::run_messages::fixtures::insert(&writer.conn, &run_record(6), 1);
        let reader = Database::open_read_only(&path).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let mut writer = Some(writer);
        reader
            .conn
            .progress_handler(
                1,
                Some(move || {
                    if seen.fetch_add(1, Ordering::Relaxed) + 1 == inject_at
                        && let Some(writer) = writer.take()
                    {
                        crate::repo::run_messages::fixtures::insert(
                            &writer.conn,
                            &run_record(9),
                            2,
                        );
                    }
                    false
                }),
            )
            .unwrap();
        let report = reader
            .inspect_check_bodies(&BodyCheckScope::All, true)
            .unwrap();
        reader
            .conn
            .progress_handler(1, None::<fn() -> bool>)
            .unwrap();
        (report, calls.load(Ordering::Relaxed), dir)
    };
    let (baseline, total, _dir) = run(usize::MAX);
    assert_eq!(baseline["ok"], true, "{baseline}");
    assert!(total > 100, "the handler must see the whole check: {total}");

    // When: the save lands at 40 different points of the check.
    for step in 1..=40 {
        let (report, _, _dir) = run(total * step / 41);

        // Then: the check reads one snapshot, so it never reports the half-seen save.
        assert_eq!(report["ok"], true, "injected at {step}/41: {report}");
    }
}
