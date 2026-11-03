use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::body_ref::BodyToken;
use crate::body_store::test_owner::{OWNERS, count, database, install, load, store, store_b, text};
use crate::repo::thread_retention::reclaim::reclaim_oversized_step;
use crate::{Database, StorageConfig};

const MIB: usize = 1024 * 1024;

fn queue_len(conn: &Connection) -> i64 {
    count(conn, "SELECT COUNT(*) FROM content_blob_gc_queue")
}

fn blob_count(conn: &Connection) -> i64 {
    count(conn, "SELECT COUNT(*) FROM content_blobs")
}

/// A queued blob row of `stored` bytes that nothing refers to. The bytes are not a
/// valid body: the collector never reads them.
fn queued_blob(conn: &Connection, seed: &str, stored: usize) -> i64 {
    let id = unqueued_blob(conn, seed, stored);
    conn.execute(
        "INSERT INTO content_blob_gc_queue(blob_id) VALUES (?1)",
        [id],
    )
    .unwrap();
    id
}

fn unqueued_blob(conn: &Connection, seed: &str, stored: usize) -> i64 {
    conn.execute(
        "INSERT INTO content_blobs(hash, byte_len, codec, data) VALUES (?1, ?2, 0, zeroblob(?2))",
        rusqlite::params![&BodyToken::of(seed).hash[..], stored as i64],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn file_database(dir: &TempDir) -> (std::path::PathBuf, Database) {
    let path = dir.path().join("blobs.db");
    let database = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    install(&database.conn);
    (path, database)
}

#[test]
fn a_step_looks_at_no_more_than_64_candidates() {
    // Given: 100 queued blobs nothing refers to.
    let conn = &database().conn;
    for index in 0..100 {
        queued_blob(conn, &format!("s{index}"), 10);
    }

    // When: one step runs.
    let progress = gc_step(conn).unwrap();

    // Then: 64 were looked at and deleted, in queue order, and 36 wait.
    assert_eq!(
        (progress.candidates, progress.deleted, progress.deferred),
        (64, 64, false)
    );
    assert_eq!((blob_count(conn), queue_len(conn)), (36, 36));
    assert_eq!(gc_step(conn).unwrap().deleted, 36);
    assert_eq!((blob_count(conn), queue_len(conn)), (0, 0));
}

#[test]
fn a_step_deletes_at_most_eight_mib_of_stored_bytes() {
    // Given: four queued blobs of 3 MiB.
    let conn = &database().conn;
    for index in 0..4 {
        queued_blob(conn, &format!("m{index}"), 3 * MIB);
    }

    // When: steps run.
    let first = gc_step(conn).unwrap();

    // Then: two fit under 8 MiB, the third waits for the next step.
    assert_eq!(
        (first.deleted, first.deleted_bytes, first.deferred),
        (2, (6 * MIB) as u64, true)
    );
    assert_eq!(queue_len(conn), 2);
    assert_eq!(gc_step(conn).unwrap().deleted, 2);
}

#[test]
fn the_four_mib_and_eight_mib_limits_are_inclusive() {
    // Given: blobs of exactly 4 MiB twice, then one byte more than 4 MiB, then 1 byte.
    let conn = &database().conn;
    queued_blob(conn, "a", 4 * MIB);
    queued_blob(conn, "b", 4 * MIB);
    queued_blob(conn, "c", 1);

    // When: a step runs.
    let first = gc_step(conn).unwrap();

    // Then: 4 + 4 MiB is allowed, the extra byte would be 8 MiB + 1 and waits.
    assert_eq!(
        (first.deleted, first.deleted_bytes, first.deferred),
        (2, (8 * MIB) as u64, true)
    );
    assert_eq!(gc_step(conn).unwrap().deleted, 1);

    // A blob of 4 MiB + 1 is never deleted by a step: it is handed to the exclusive
    // phase, and leaves the queue so it cannot starve the blobs behind it.
    let big = queued_blob(conn, "big", 4 * MIB + 1);
    queued_blob(conn, "small", 10);
    let progress = gc_step(conn).unwrap();
    assert_eq!(
        (
            progress.deleted,
            progress.registered_oversized,
            progress.candidates
        ),
        (1, 1, 2)
    );
    assert_eq!(queue_len(conn), 0);
    assert_eq!(blob_count(conn), 1);
    let registered: (String, String, String, i64) = conn
        .query_row(
            "SELECT family_key, tbl, key, stored_bytes FROM reclaim_oversized",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        registered,
        (
            OVERSIZED_FAMILY.into(),
            OVERSIZED_TABLE.into(),
            big.to_string(),
            (4 * MIB + 1) as i64
        )
    );
    // The next tick has nothing to look at: the oversized blob is not examined again.
    assert_eq!(gc_step(conn).unwrap(), GcProgress::default());
}

#[test]
fn the_exclusive_phase_deletes_an_unreferenced_oversized_blob_and_spares_a_reused_one() {
    // Given: two oversized blobs registered by a step; one gets referenced again
    // before the exclusive phase runs.
    let conn = &database().conn;
    let doomed = queued_blob(conn, "doomed", 5 * MIB);
    let reused = queued_blob(conn, "reused", 5 * MIB);
    assert_eq!(gc_step(conn).unwrap().registered_oversized, 2);
    conn.execute("INSERT INTO test_owner(id, body) VALUES ('o', '[]')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO content_blob_refs_test_owner(owner_id, blob_id) VALUES ('o', ?1)",
        [reused],
    )
    .unwrap();

    // When: the exclusive phase works off the registrations, repeating until none remain.
    let mut deleted = 0;
    loop {
        let step = reclaim_oversized_step(conn, 64 * MIB as u64).unwrap();
        assert_eq!(step.last_error, None);
        deleted += step.rows;
        if step.remaining == 0 {
            break;
        }
    }

    // Then: only the unreferenced one is gone, the reused one stays with its reference,
    // and no registration or queue entry is left.
    assert_eq!(deleted, 1);
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM content_blobs")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids, vec![reused]);
    assert_ne!(ids, vec![doomed]);
    assert_eq!(count(conn, "SELECT COUNT(*) FROM reclaim_oversized"), 0);
    assert_eq!(queue_len(conn), 0);
}

#[test]
fn a_registration_for_a_blob_that_is_already_gone_is_dropped() {
    let conn = &database().conn;
    conn.execute(
        "INSERT INTO reclaim_oversized(family_key, tbl, key, stored_bytes) VALUES (?1, ?2, '777', 5)",
        [OVERSIZED_FAMILY, OVERSIZED_TABLE],
    )
    .unwrap();
    let step = reclaim_oversized_step(conn, MIB as u64).unwrap();
    assert_eq!((step.rows, step.remaining, step.last_error), (0, 0, None));
}

#[test]
fn stale_and_re_referenced_candidates_are_dropped_from_the_queue_without_deleting() {
    // Given: a queued blob that an owner references again, and a queue entry whose
    // blob is gone.
    let conn = &database().conn;
    let body = text("again", 3_000);
    store(conn, "o1", &json!([body.clone()])).unwrap();
    conn.execute("DELETE FROM test_owner WHERE id = 'o1'", [])
        .unwrap();
    store(conn, "o2", &json!([body.clone()])).unwrap();
    conn.execute(
        "INSERT INTO content_blob_gc_queue(blob_id) VALUES (999)",
        [],
    )
    .unwrap();
    assert_eq!(queue_len(conn), 2);

    // When: a step runs.
    let progress = gc_step(conn).unwrap();

    // Then: the referenced blob is kept, the stale entry is dropped, the queue is empty.
    assert_eq!(
        (progress.retained, progress.stale, progress.deleted),
        (1, 1, 0)
    );
    assert_eq!(queue_len(conn), 0);
    assert_eq!(load(conn, "o2").unwrap(), json!([body]));
}

#[test]
fn a_blob_shared_by_two_owners_survives_until_the_last_reference_goes() {
    let conn = &database().conn;
    let shared = text("shared", 3_000);
    store(conn, "o1", &json!([shared.clone()])).unwrap();
    store_b(conn, 1, &json!([shared.clone()])).unwrap();

    conn.execute("DELETE FROM test_owner WHERE id = 'o1'", [])
        .unwrap();
    assert_eq!(gc_step(conn).unwrap().retained, 1);
    assert_eq!(blob_count(conn), 1);

    conn.execute("DELETE FROM test_owner_b WHERE n = 1", [])
        .unwrap();
    assert_eq!(gc_step(conn).unwrap().deleted, 1);
    assert_eq!(blob_count(conn), 0);
}

#[test]
fn a_reference_table_nobody_registered_still_protects_its_blobs() {
    // Given: a table that follows the naming but is not in the registry, holding a
    // reference to a queued blob.
    let conn = &database().conn;
    let id = queued_blob(conn, "guarded", 100);
    conn.execute_batch(
        "CREATE TABLE content_blob_refs_unregistered (
             blob_id INTEGER NOT NULL REFERENCES content_blobs(id));
         CREATE INDEX idx_unregistered_blob ON content_blob_refs_unregistered(blob_id);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO content_blob_refs_unregistered VALUES (?1)",
        [id],
    )
    .unwrap();

    // When: a step runs.
    let progress = gc_step(conn).unwrap();

    // Then: the blob is kept, and the checker reports the table as unregistered.
    assert_eq!((progress.deleted, progress.retained), (0, 1));
    let report = check_bodies(conn, OWNERS, BodyScope::All, false).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.kind == "unregistered_ref_table"),
        "{:?}",
        report.problems
    );
}

#[test]
fn a_failure_inside_the_step_rolls_back_every_delete_and_queue_update() {
    // Given: three queued blobs, and a delete that fails on the second.
    let conn = &database().conn;
    let ids: Vec<i64> = (0..3)
        .map(|i| queued_blob(conn, &format!("x{i}"), 50))
        .collect();
    conn.execute_batch(&format!(
        "CREATE TRIGGER inject BEFORE DELETE ON content_blobs WHEN OLD.id = {}
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        ids[1]
    ))
    .unwrap();

    // When: the step runs.
    let error = gc_step(conn).unwrap_err();

    // Then: it failed as a whole: all blobs and all queue entries are still there.
    assert!(error.to_string().contains("injected"), "{error}");
    assert_eq!((blob_count(conn), queue_len(conn)), (3, 3));

    // And the next step, with the fault gone, collects them.
    conn.execute_batch("DROP TRIGGER inject").unwrap();
    assert_eq!(gc_step(conn).unwrap().deleted, 3);
}

#[test]
fn an_empty_step_takes_no_write_lock_and_writes_nothing() {
    // Given: a file database whose write lock another connection holds.
    let dir = tempfile::tempdir().unwrap();
    let (path, database) = file_database(&dir);
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    database
        .conn
        .busy_timeout(std::time::Duration::ZERO)
        .unwrap();
    let before = database.conn.total_changes();

    // When: a step runs with an empty queue.
    let progress = gc_step(&database.conn).unwrap();

    // Then: it neither waited for nor needed the lock, and changed nothing.
    assert_eq!(progress, GcProgress::default());
    assert_eq!(database.conn.total_changes(), before);

    // With work queued it does need the lock, and reports the contention.
    holder.execute_batch("ROLLBACK").unwrap();
    queued_blob(&database.conn, "q", 10);
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(gc_step(&database.conn).is_err());
    holder.execute_batch("ROLLBACK").unwrap();
    assert_eq!(gc_step(&database.conn).unwrap().deleted, 1);
}

#[test]
fn the_deleting_statement_and_the_checks_probe_by_index_only() {
    let conn = &database().conn;
    let tables = ref_tables(conn).unwrap();
    assert_eq!(tables.len(), OWNERS.len());
    for sql in [
        format!(
            "DELETE FROM content_blobs WHERE id = ?1{}",
            unreferenced_clause(&tables, "?1")
        ),
        format!(
            "SELECT 1 FROM content_blobs b WHERE 1{}",
            unreferenced_clause(&tables, "b.id")
        ),
    ] {
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", sql.replace("?1", "1")))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for table in &tables {
            let step = plan
                .iter()
                .find(|step| step.contains(table.as_str()))
                .unwrap_or_else(|| panic!("{table} not probed: {plan:?}"));
            assert!(step.contains("USING"), "{step}");
        }
    }
}

mod check {
    use super::super::check::CheckReport;
    use super::*;
    use crate::body_store::test_owner::store_in_run;

    fn kinds(report: &CheckReport) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> =
            report.problems.iter().map(|problem| problem.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    }

    fn check(conn: &Connection, verify: bool) -> CheckReport {
        check_bodies(conn, OWNERS, BodyScope::All, verify).unwrap()
    }

    fn fixture() -> Database {
        let database = database();
        let conn = &database.conn;
        store(
            conn,
            "o1",
            &json!([text("a", 3_000), text("b", 3_000), "plain"]),
        )
        .unwrap();
        store_b(conn, 1, &json!({ "k": text("a", 3_000) })).unwrap();
        database
    }

    #[test]
    fn a_consistent_database_passes_with_and_without_verification() {
        let database = fixture();
        for verify in [false, true] {
            let report = check(&database.conn, verify);
            assert_eq!(report.problems, vec![]);
            assert!(report.is_ok());
            assert_eq!(
                (report.owners_checked, report.owner_rows, report.tokens),
                (OWNERS.len(), 2, 3)
            );
            assert_eq!(report.blobs_verified, if verify { 2 } else { 0 });
        }
    }

    #[test]
    fn a_blob_nothing_names_is_waiting_when_queued_and_an_orphan_when_not() {
        let database = fixture();
        let conn = &database.conn;
        queued_blob(conn, "queued", 10);
        let report = check(conn, false);
        assert!(report.is_ok());
        assert_eq!(report.waiting_for_collection, 1);

        unqueued_blob(conn, "lost", 10);
        let report = check(conn, false);
        assert_eq!(kinds(&report), vec!["orphan_blob"]);
        assert!(!report.is_ok());
    }

    #[test]
    fn missing_surplus_dangling_and_mismatching_references_are_each_reported() {
        let database = fixture();
        let conn = &database.conn;
        let a = BodyToken::of(&text("a", 3_000));
        let id_of = |token: &BodyToken| -> i64 {
            conn.query_row(
                "SELECT id FROM content_blobs WHERE hash = ?1",
                [&token.hash[..]],
                |row| row.get(0),
            )
            .unwrap()
        };

        // A reference row lost while the row still names the blob.
        conn.execute(
            "DELETE FROM content_blob_refs_test_owner WHERE owner_id = 'o1' AND blob_id = ?1",
            [id_of(&a)],
        )
        .unwrap();
        assert_eq!(kinds(&check(conn, false)), vec!["missing_reference"]);

        // A reference row to a blob the row does not name.
        let stray = unqueued_blob(conn, "stray", 5);
        conn.execute(
            "INSERT INTO content_blob_refs_test_owner(owner_id, blob_id) VALUES ('o1', ?1)",
            [stray],
        )
        .unwrap();
        assert_eq!(
            kinds(&check(conn, false)),
            vec!["missing_reference", "surplus_reference"]
        );

        // A token whose length differs from its blob's.
        conn.execute(
            "UPDATE content_blobs SET byte_len = byte_len + 1 WHERE hash = ?1",
            [&a.hash[..]],
        )
        .unwrap();
        assert!(kinds(&check(conn, false)).contains(&"length_mismatch"));

        // The blob itself is gone (foreign keys off): the row names a blob that does not
        // exist, and a reference row points at nothing.
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute("DELETE FROM content_blobs WHERE hash = ?1", [&a.hash[..]])
            .unwrap();
        let found = kinds(&check(conn, false));
        for kind in [
            "blob_missing",
            "dangling_reference",
            "foreign_key_violation",
        ] {
            assert!(found.contains(&kind), "{kind} not in {found:?}");
        }
    }

    #[test]
    fn tokens_naming_one_blob_with_different_lengths_are_reported_in_either_order() {
        // Given: a stored row whose container is then edited to hold two tokens for the
        // same blob, one with a wrong length, in each order.
        let database = database();
        let conn = &database.conn;
        let body = text("dup", 3_000);
        store(conn, "o1", &json!([body.clone()])).unwrap();
        let good = BodyToken::of(&body);
        let bad = BodyToken {
            byte_len: 2_999,
            ..good
        };
        let (good_text, bad_text) = (good.to_value_string(), bad.to_value_string());

        for (order, first, second) in [
            ("bad first", &bad_text, &good_text),
            ("bad last", &good_text, &bad_text),
        ] {
            conn.execute(
                "UPDATE test_owner SET body = ?1 WHERE id = 'o1'",
                [json!([first, second]).to_string()],
            )
            .unwrap();

            for verify in [false, true] {
                // When: the database is checked.
                let report = check(conn, verify);

                // Then: the conflict and the wrong length are both findings naming the
                // row and the blob, and the check fails.
                let found = kinds(&report);
                assert!(!report.is_ok(), "{order}, verify={verify}");
                for kind in ["token_length_conflict", "length_mismatch"] {
                    assert!(found.contains(&kind), "{order}, verify={verify}: {found:?}");
                }
                for problem in &report.problems {
                    assert_eq!(problem.owner.as_deref(), Some("test_owner owner_id=o1"));
                    assert_eq!(problem.hash.as_deref(), Some(good.hash_hex().as_str()));
                }
            }
        }

        // The same token twice with the right length is no finding.
        conn.execute(
            "UPDATE test_owner SET body = ?1 WHERE id = 'o1'",
            [json!([good_text, good_text]).to_string()],
        )
        .unwrap();
        assert!(check(conn, true).is_ok());
    }

    #[test]
    fn unreadable_containers_and_tokens_are_reported_not_skipped() {
        let database = fixture();
        let conn = &database.conn;
        conn.execute("UPDATE test_owner SET body = '{not json'", [])
            .unwrap();
        assert_eq!(kinds(&check(conn, false)), vec!["container_json"]);
        conn.execute("UPDATE test_owner SET body = ?1", ["[\"\u{e000}broken\"]"])
            .unwrap();
        assert_eq!(kinds(&check(conn, false)), vec!["invalid_token"]);
    }

    #[test]
    fn verification_finds_bytes_that_do_not_match_their_hash() {
        // Given: a blob whose stored bytes were replaced by other bytes of the same length.
        let database = database();
        let conn = &database.conn;
        store(conn, "o", &json!("\u{e000}x")).unwrap();
        conn.execute(
            "UPDATE content_blobs SET data = CAST(?1 AS BLOB)",
            ["\u{e000}y"],
        )
        .unwrap();

        // Then: structure alone is fine; verification reports it, by owner and by sweep.
        assert!(check(conn, false).is_ok());
        let report = check(conn, true);
        assert_eq!(kinds(&report), vec!["blob_unreadable"]);
        assert!(report.problems[0].detail.contains("hash"));
        assert!(
            report
                .problems
                .iter()
                .all(|p| !p.detail.contains('\u{e000}'))
        );
    }

    #[test]
    fn a_scope_checks_only_the_rows_of_that_run_or_thread() {
        // Given: two runs, one of them broken, in two threads.
        let database = database();
        let conn = &database.conn;
        store_in_run(conn, "good", Some("run-good"), &json!([text("g", 3_000)])).unwrap();
        store_in_run(conn, "bad", Some("run-bad"), &json!([text("b", 3_000)])).unwrap();
        conn.execute_batch(
            "INSERT INTO threads(thread_id, project_root, root_run_id, archived, created_at_ns,
                 updated_at_ns, source) VALUES ('t-good', '/p', 'run-good', 0, 1, 1, 'host'),
                 ('t-bad', '/p', 'run-bad', 0, 1, 1, 'host');
             INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth)
                 VALUES ('run-good', 't-good', 'run-good', 0), ('run-bad', 't-bad', 'run-bad', 0);
             DELETE FROM content_blob_refs_test_owner WHERE owner_id = 'bad';",
        )
        .unwrap();

        // Then: each scope sees only its own rows; an unscoped owner is left out of a
        // scoped check.
        let run = |scope| check_bodies(conn, OWNERS, scope, false).unwrap();
        assert!(run(BodyScope::Run("run-good")).is_ok());
        assert_eq!(
            kinds(&run(BodyScope::Run("run-bad"))),
            vec!["missing_reference"]
        );
        assert!(run(BodyScope::Thread("t-good")).is_ok());
        assert_eq!(
            kinds(&run(BodyScope::Thread("t-bad"))),
            vec!["missing_reference"]
        );
        // Only owners that can say which run a row belongs to take part in a scoped check.
        assert_eq!(
            run(BodyScope::Run("run-good")).owners_checked,
            OWNERS
                .iter()
                .filter(|owner| owner.run_column.is_some())
                .count()
        );
        assert_eq!(kinds(&run(BodyScope::All)), vec!["missing_reference"]);
    }

    #[test]
    fn rows_outside_an_owners_token_filter_are_not_interpreted() {
        let database = database();
        let conn = &database.conn;
        conn.execute(
            "INSERT INTO test_owner_b(n, body, tokenized) VALUES (9, ?1, 0)",
            ["[\"\u{e000}raw, not a token\"]"],
        )
        .unwrap();
        assert!(check(conn, true).is_ok());
    }

    #[test]
    fn a_reference_table_without_the_template_is_reported() {
        let database = database();
        let conn = &database.conn;
        conn.execute_batch("CREATE TABLE content_blob_refs_bad (blob_id INTEGER);")
            .unwrap();
        let found = kinds(&check(conn, false));
        for kind in [
            "unregistered_ref_table",
            "ref_table_index_missing",
            "ref_table_trigger_missing",
        ] {
            assert!(found.contains(&kind), "{kind} not in {found:?}");
        }
        conn.execute_batch(
            "DROP TABLE content_blob_refs_bad; DROP TABLE content_blob_refs_test_owner;",
        )
        .unwrap();
        assert!(kinds(&check(conn, false)).contains(&"owner_ref_table_missing"));
    }

    #[test]
    fn blob_lookup_by_prefix_names_one_blob_and_rejects_ambiguity() {
        let database = database();
        let conn = &database.conn;
        for last in [0x10_u8, 0x11] {
            let mut hash = [0xab_u8; 32];
            hash[31] = last;
            conn.execute(
                "INSERT INTO content_blobs(hash, byte_len, codec, data) VALUES (?1, 1, 0, 'x')",
                [&hash[..]],
            )
            .unwrap();
        }
        let prefix = "ab".repeat(15);
        assert_eq!(find_blobs(conn, &prefix).unwrap().len(), 2);
        let full = format!("{}10", "ab".repeat(31));
        assert_eq!(find_blobs(conn, &full).unwrap().len(), 1);
        assert_eq!(
            find_blobs(conn, &format!("{}1", "ab".repeat(31)))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            find_blobs(conn, &format!("{}12", "ab".repeat(31)))
                .unwrap()
                .len(),
            0
        );
        for bad in ["", "AB12", "xyz", &"a".repeat(65)] {
            assert!(find_blobs(conn, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn body_statistics_count_blobs_references_sharing_and_waiting_blobs() {
        let database = fixture();
        let conn = &database.conn;
        queued_blob(conn, "waiting", 10);
        unqueued_blob(conn, "orphan", 10);
        let stats = body_stats(conn).unwrap();
        assert_eq!(stats["blobs"], 4);
        assert_eq!(stats["shared_blobs"], 1);
        assert_eq!(stats["unreferenced"]["waiting_in_queue"], 1);
        assert_eq!(stats["unreferenced"]["unregistered_orphans"], 1);
        assert_eq!(stats["gc_queue_entries"], 1);
        let rows: i64 = stats["references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|table| table["rows"].as_i64().unwrap())
            .sum();
        assert_eq!(rows, 3);
    }
}

#[test]
fn the_migration_creates_each_production_reference_table_in_the_shape_of_the_template() {
    // The migration text is written by hand; the template is what the collector and
    // the check rely on, so the two must say the same thing.
    let squash = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let migration = squash(include_str!("../../migrations/v40_prepare.sql"));
    for owner in crate::body_store::PRODUCTION_OWNERS {
        let template = squash(&owner.ref_table_ddl());
        assert!(
            migration.contains(&template),
            "{} differs from its template:\n{template}",
            owner.ref_table
        );
    }
}
