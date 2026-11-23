use std::collections::HashSet;

use event_bus::{Event, EventKind};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::committed::{
    EventStamp, IDS_COMMITTED_BETWEEN_SQL, IDS_COMMITTED_OF_RUN_SQL, IDS_UNCONVERTED_BETWEEN_SQL,
    IDS_UNCONVERTED_OF_RUN_SQL, NEWEST_COMMITTED_SQL, NEWEST_UNCONVERTED_SQL,
};
use super::deferred::DeferredRow;
use super::*;
use crate::host_input::{HostInputProvenance, HostInputReceipt, HostInputStatus};
use crate::{Database, StorageConfig};

fn kind(category: &str, variant: &str, payload: Value) -> EventKind {
    let value = json!({"kind": category, "payload": {"kind": variant, "payload": payload}});
    serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("{error}: {value}"))
}

fn answer(run: &str) -> EventKind {
    kind(
        "Message",
        "MessageCompleted",
        json!({"run_id": run, "text": "a"}),
    )
}

fn final_result(run: &str) -> EventKind {
    kind(
        "Message",
        "FinalResultPublished",
        json!({"run_id": run, "text": "f"}),
    )
}

fn part(run: &str) -> EventKind {
    kind(
        "Message",
        "MessagePart",
        json!({"run_id": run, "response_id": "r", "text": "p", "index": 0, "total": 2}),
    )
}

fn prompt(run: &str) -> EventKind {
    kind(
        "Lifecycle",
        "TaskPromptPublished",
        json!({"run_id": run, "agent_name": "w", "role": "Worker", "parent_run_id": null,
               "prompt": "do", "user_text": null, "skills": []}),
    )
}

fn tool(run: Option<&str>) -> EventKind {
    kind(
        "Tool",
        "ToolCompleted",
        json!({"tool_name": "t", "call_id": "c", "is_error": false, "output": "o", "run_id": run}),
    )
}

fn delta(run: &str) -> EventKind {
    kind(
        "Message",
        "MessageDelta",
        json!({"delta": "d", "run_id": run}),
    )
}

fn append(conn: &Connection, kind: &EventKind) -> i64 {
    let event = Event {
        meta: event_bus::EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_nanos(7),
            wall_clock: std::time::UNIX_EPOCH + Duration::from_secs(10),
        },
        kind: kind.clone(),
    };
    append_event(
        conn,
        Some("session"),
        &event,
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap()
    .id
}

fn insert_unconverted(conn: &Connection, kind: &EventKind) -> i64 {
    conn.execute(
        "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
         VALUES (NULL, 1, 1, 10000000000, ?1, ?2)",
        params![kind_name(kind), serde_json::to_string(kind).unwrap()],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_unreadable(conn: &Connection, subkind: Option<&str>, run: Option<&str>) -> i64 {
    conn.execute(
        "INSERT INTO events (schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, \
         run_id) VALUES (1, 1, 1, 'Message', '{\"kind\":\"Removed\"}', ?1, ?2)",
        params![subkind, run],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn tombstone(conn: &Connection, run: &str) {
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES (?1, 'thread', 'family')",
        [run],
    )
    .unwrap();
}

fn label(row: &DeferredRow) -> (i64, bool) {
    match row {
        DeferredRow::Event(event) => (event.id, false),
        DeferredRow::Failed(failure) => (failure.id, true),
    }
}

fn after(
    db: &Database,
    runs: &[&str],
    after: i64,
    through: i64,
) -> (Vec<(i64, bool)>, Option<i64>) {
    let runs: HashSet<&str> = runs.iter().copied().collect();
    let mut rows = Vec::new();
    let newest = db
        .for_each_committed_event_after(&runs, after, through, |row| rows.push(label(&row)))
        .unwrap();
    (rows, newest.map(|(id, _)| id))
}

fn of_run(db: &Database, run: &str, through: i64) -> Vec<(i64, bool)> {
    let mut rows = Vec::new();
    db.for_each_committed_event_of_run(run, through, |row| rows.push(label(&row)))
        .unwrap();
    rows
}

fn events(ids: &[i64]) -> Vec<(i64, bool)> {
    ids.iter().map(|&id| (id, false)).collect()
}

#[test]
fn the_window_holds_only_committed_rows_of_the_listed_runs_and_only_they_are_fetched() {
    // Given: three runs whose committed rows of every kind interleave with deltas.
    let dir = tempfile::TempDir::new().unwrap();
    let db = Database::open(&StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    })
    .unwrap();
    let conn = &db.conn;
    let a1 = append(conn, &prompt("run-a"));
    let _ = append(conn, &delta("run-a"));
    let b1 = append(conn, &answer("run-b"));
    let a2 = append(conn, &part("run-a"));
    let c1 = append(conn, &answer("run-c"));
    let _ = append(conn, &delta("run-b"));
    let a3 = append(conn, &tool(Some("run-a")));
    let orphan = append(conn, &tool(None));
    let b2 = append(conn, &final_result("run-b"));
    let a4 = append(conn, &answer("run-a"));
    let c2 = append(conn, &answer("run-c"));
    let tail = append(conn, &delta("run-a"));
    let meter = crate::test_support::meter_event_rows(&db);

    // When/Then: the bounds are exclusive below and inclusive above.
    let before = meter.counts().rows_fetched;
    let (rows, newest) = after(&db, &["run-a", "run-b"], a1, b2);
    assert_eq!(rows, events(&[b1, a2, a3, b2]));
    assert_eq!(newest, Some(b2));
    // Every committed row of the window is fetched, whatever its run, and no delta is.
    assert_eq!(
        meter.counts().rows_fetched - before,
        [b1, a2, c1, a3, orphan, b2].len() as u64
    );

    let (rows, newest) = after(&db, &["run-a"], a4, tail);
    assert_eq!(rows, []);
    assert_eq!(newest, Some(c2));
    assert_eq!(after(&db, &["run-a"], tail, i64::MAX), (vec![], None));
    assert_eq!(
        after(&db, &["run-a"], 0, i64::MAX).0,
        events(&[a1, a2, a3, a4])
    );

    let before = meter.counts().rows_fetched;
    assert_eq!(of_run(&db, "run-a", a3), events(&[a1, a2, a3]));
    assert_eq!(meter.counts().rows_fetched - before, 3);
    assert_eq!(of_run(&db, "run-b", i64::MAX), events(&[b1, b2]));
    assert_eq!(db.max_committed_id().unwrap().map(|(id, _)| id), Some(c2));

    let before = meter.counts().rows_fetched;
    db.for_each_event_for_runs_deferred(&["run-b".to_owned()], 0, i64::MAX, |_| {})
        .unwrap();
    assert_eq!(meter.counts().rows_fetched - before, 3);
}

#[test]
fn unbackfilled_rows_are_confirmed_tombstones_skipped_and_failures_named_by_column() {
    // Given: converted and unconverted committed rows, a deleted run, and unreadable rows.
    let db = Database::open_in_memory().unwrap();
    let conn = &db.conn;
    let a1 = append(conn, &answer("run-a"));
    let legacy_a = insert_unconverted(conn, &final_result("run-a"));
    let _legacy_b = insert_unconverted(conn, &answer("run-b"));
    let _legacy_delta = insert_unconverted(conn, &delta("run-a"));
    let broken_a = insert_unreadable(conn, Some("MessageCompleted"), Some("run-a"));
    let _broken_b = insert_unreadable(conn, Some("MessageCompleted"), Some("run-b"));
    let _broken_legacy = insert_unreadable(conn, None, None);
    let _gone = append(conn, &answer("run-gone"));
    let _broken_gone = insert_unreadable(conn, Some("MessageCompleted"), Some("run-gone"));
    let a2 = append(conn, &part("run-a"));
    let legacy_gone = insert_unconverted(conn, &answer("run-gone"));
    tombstone(conn, "run-gone");

    // When/Then: both readers agree, the failure of run-a is in its place, and nothing
    // of the deleted run or of an unlisted run is handed over.
    let expected = vec![
        (a1, false),
        (legacy_a, false),
        (broken_a, true),
        (a2, false),
    ];
    let (rows, newest) = after(&db, &["run-a", "run-gone"], 0, i64::MAX);
    assert_eq!(rows, expected);
    assert_eq!(of_run(&db, "run-a", i64::MAX), expected);
    assert_eq!(of_run(&db, "run-gone", i64::MAX), []);
    assert_eq!(newest, Some(a2));
    assert!(legacy_gone > a2);
    assert_eq!(db.max_committed_id().unwrap().map(|(id, _)| id), Some(a2));

    let legacy_newest = insert_unconverted(conn, &tool(Some("run-b")));
    let _ = insert_unconverted(conn, &delta("run-b"));
    let (newest, stamp) = db.max_committed_id().unwrap().unwrap();
    assert_eq!(newest, legacy_newest);
    assert_eq!(
        stamp,
        EventStamp {
            session_id: None,
            monotonic_ns: 1,
            wall_clock_ns: 10_000_000_000
        }
    );
}

#[test]
fn stamps_tell_a_replaced_row_from_the_original() {
    let db = Database::open_in_memory().unwrap();
    let conn = &db.conn;
    let first = append(conn, &answer("run-a"));
    let second = append(conn, &answer("run-a"));
    let stamps = db.event_stamps(&[second, first, second + 1]).unwrap();
    assert_eq!(
        stamps.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [second, first]
    );
    let original = stamps[0].1.clone();
    assert_eq!(
        original,
        EventStamp {
            session_id: Some("session".into()),
            monotonic_ns: 7,
            wall_clock_ns: 10_000_000_000
        }
    );
    assert_eq!(
        db.max_committed_id().unwrap(),
        Some((second, original.clone()))
    );

    conn.execute("DELETE FROM events WHERE id = ?1", [second])
        .unwrap();
    let reused = insert_unconverted(conn, &answer("run-a"));
    assert_eq!(
        reused, second,
        "SQLite reuses the largest rowid once it is deleted"
    );
    assert_ne!(db.event_stamps(&[second]).unwrap()[0].1, original);
}

fn plan(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> String {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows: Vec<String> = statement
        .query_map(params, |row| row.get::<_, String>(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.join(" | ")
}

#[test]
fn the_committed_readers_search_indexes_instead_of_scanning_events() {
    let db = Database::open_in_memory().unwrap();
    let conn = &db.conn;
    let (kind, subkind, run, id) = ("Message", "MessageCompleted", "run-a", 5_i64);
    for (sql, params, index) in [
        (
            IDS_COMMITTED_BETWEEN_SQL,
            vec![&kind as &dyn rusqlite::ToSql, &subkind, &id, &id],
            "COVERING INDEX idx_events_kind_subkind_id",
        ),
        (
            IDS_UNCONVERTED_BETWEEN_SQL,
            vec![&id as &dyn rusqlite::ToSql, &id],
            "idx_events_unbackfilled",
        ),
        (
            IDS_COMMITTED_OF_RUN_SQL,
            vec![&run as &dyn rusqlite::ToSql, &id],
            "idx_events_run_id",
        ),
        (
            IDS_UNCONVERTED_OF_RUN_SQL,
            vec![&run as &dyn rusqlite::ToSql, &id],
            "SEARCH events USING INDEX",
        ),
        (
            NEWEST_COMMITTED_SQL,
            vec![&kind as &dyn rusqlite::ToSql, &subkind],
            "idx_events_kind_subkind_id",
        ),
        (
            NEWEST_UNCONVERTED_SQL,
            vec![&id as &dyn rusqlite::ToSql],
            "idx_events_unbackfilled",
        ),
    ] {
        let plan = plan(conn, sql, &params);
        assert!(plan.contains(index), "{sql}: {plan}");
        assert!(!plan.contains("SCAN events"), "{sql}: {plan}");
    }
}

fn receipt(run: &str, id: &str, text: &str, at: i64, status: HostInputStatus) -> HostInputReceipt {
    HostInputReceipt {
        provenance: HostInputProvenance::User,
        prompt_id: id.into(),
        run_id: run.into(),
        root_run_id: run.into(),
        text: text.into(),
        status,
        reason: None,
        accepted_at_ns: at,
        status_at_ns: at,
        evidence_id: None,
    }
}

#[test]
fn delivered_headers_of_all_runs_match_each_runs_delivered_history() {
    // Given: run-1 delivers only a follow-up accepted after its undelivered initial input
    // at the same time; run-2 mixes equal times, a drop and a notice; run-gone is tombstoned.
    let db = Database::open_in_memory().unwrap();
    let conn = &db.conn;
    let large = "x".repeat(16_385);
    let notice = HostInputReceipt {
        provenance: HostInputProvenance::HarnessNotice {
            notification_id: "n".into(),
            destination_thread_id: "t".into(),
        },
        ..receipt("run-2", "notice", "", 1, HostInputStatus::Delivered)
    };
    for receipt in [
        receipt(
            "run-1",
            "z-initial",
            "initial",
            10,
            HostInputStatus::Accepted,
        ),
        receipt(
            "run-1",
            "a-followup",
            "followup",
            10,
            HostInputStatus::Delivered,
        ),
        notice,
        receipt("run-2", "m-first", "first", 5, HostInputStatus::Delivered),
        receipt("run-2", "b-same", &large, 5, HostInputStatus::Delivered),
        receipt("run-2", "dropped", "gone", 7, HostInputStatus::Dropped),
        receipt(
            "run-gone",
            "deleted",
            "deleted",
            1,
            HostInputStatus::Delivered,
        ),
    ] {
        crate::repo::host_input_receipt::insert(conn, &receipt).unwrap();
    }
    tombstone(conn, "run-gone");

    // When: every run's headers are read at once.
    let headers = db.delivered_host_input_headers().unwrap();

    // Then: they are exactly the per-run histories without their text, run by run.
    let mut expected = Vec::new();
    for run in ["run-1", "run-2", "run-gone"] {
        for input in db.delivered_host_input_history(run).unwrap() {
            expected.push(crate::host_input::DeliveredHostInputHeader {
                prompt_id: input.prompt_id,
                run_id: input.run_id,
                accepted_at_ns: input.accepted_at_ns,
                text_bytes: input.text_bytes,
                first_in_run: input.first_in_run,
            });
        }
    }
    assert_eq!(headers, expected);
    let firsts: Vec<_> = headers
        .iter()
        .map(|h| (h.prompt_id.as_str(), h.first_in_run))
        .collect();
    assert_eq!(
        firsts,
        [("a-followup", false), ("b-same", false), ("m-first", true)]
    );
    assert_eq!(headers[1].text_bytes, 16_385);

    let ids = ["m-first", "deleted", "b-same", "missing", "a-followup"].map(String::from);
    assert_eq!(
        db.delivered_host_input_texts(&ids).unwrap(),
        [
            ("m-first".to_owned(), Some("first".to_owned())),
            ("b-same".to_owned(), None),
            ("a-followup".to_owned(), Some("followup".to_owned())),
        ]
    );
}
