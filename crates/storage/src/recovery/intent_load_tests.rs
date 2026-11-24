//! Pinned behaviour of the recovery-time scan of `resume_intents`.
//!
//! The expectations below are written out by hand from the scan's contract (tombstoned
//! runs are skipped, rows come back in `run_id` order, a row that cannot be read keeps the
//! identity that *can* be read, and later rows are still delivered). They are never
//! regenerated from the output of the code under test.
use super::*;
use rusqlite::types::Type;

const BIG: usize = 1024 * 1024;

fn big_payload() -> String {
    format!("{{\"filler\":\"{}\"}}", "x".repeat(BIG))
}

/// Rows are inserted in a scrambled order on purpose: only `ORDER BY run_id` may
/// decide the order of the scan.
fn database() -> Database {
    let db = Database::open_in_memory().unwrap();
    let c = &db.conn;
    let ok = |id: &str, incarnation: &str, payload: &str| {
        c.execute(
            "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,?2,?3)",
            params![id, incarnation, payload],
        )
        .unwrap();
    };
    ok("run-c", "inc-c", "{\"c\":true}");
    ok("run-a", "inc-a", &big_payload());
    // Tombstoned after the insert (the insert trigger refuses a tombstoned run).
    ok("run-tombstoned", "inc-t", "{\"t\":true}");
    c.execute(
        "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES('run-tombstoned','thread','family')",
        [],
    )
    .unwrap();
    ok("run-b", "inc-b", "not even json");
    // The relational type is wrong: TEXT affinity keeps a BLOB as a BLOB.
    c.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES('run-bad-incarnation',X'0102','{}')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES('run-bad-payload','inc-p',X'0304')",
        [],
    )
    .unwrap();
    // Not even the identity is readable. BLOB sorts after every TEXT value.
    c.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(X'05',X'06','{}')",
        [],
    )
    .unwrap();
    db
}

/// The only contract a scan result has: this exact sequence.
fn assert_pinned(loads: &[RecoveryIntentLoad]) {
    assert_eq!(
        loads.len(),
        6,
        "tombstoned run excluded, nothing else dropped"
    );
    let readable = |index: usize, id: &str, incarnation: &str, payload: &str| {
        let (read_id, record) = &loads[index];
        assert_eq!(read_id.as_deref(), Some(id), "row {index}");
        let record = record
            .as_ref()
            .unwrap_or_else(|e| panic!("row {index}: {e:?}"));
        assert_eq!(record.run_id, id, "row {index}");
        assert_eq!(record.incarnation, incarnation, "row {index}");
        assert_eq!(record.payload, payload, "row {index}");
    };
    readable(0, "run-a", "inc-a", &big_payload());
    readable(1, "run-b", "inc-b", "not even json");
    // The relational error keeps the identity that was readable.
    for (index, id, column, kind) in [
        (2, "run-bad-incarnation", 1, Type::Blob),
        (3, "run-bad-payload", 2, Type::Blob),
    ] {
        assert_eq!(loads[index].0.as_deref(), Some(id), "row {index}");
        match &loads[index].1 {
            Err(StorageError::Sqlite(rusqlite::Error::InvalidColumnType(c, _, t))) => {
                assert_eq!((*c, t), (column, &kind), "row {index}");
            }
            other => panic!("row {index}: expected a column type error, got {other:?}"),
        }
    }
    // A later healthy row is still delivered after unreadable ones.
    readable(4, "run-c", "inc-c", "{\"c\":true}");
    assert_eq!(
        loads[5].0, None,
        "an unreadable identity is reported as None"
    );
    assert!(matches!(
        &loads[5].1,
        Err(StorageError::Sqlite(rusqlite::Error::InvalidColumnType(
            0,
            _,
            Type::Blob
        )))
    ));
}

#[test]
fn recovery_intent_loads_pins_order_isolation_and_tombstones() {
    let db = database();
    assert_pinned(&db.recovery_intent_loads().unwrap());
}

#[test]
fn a_scan_that_cannot_start_is_an_error_not_a_partial_result() {
    let db = Database::open_in_memory().unwrap();
    db.conn.execute_batch("DROP TABLE resume_intents").unwrap();
    assert!(matches!(
        db.recovery_intent_loads(),
        Err(StorageError::Sqlite(_))
    ));
}
