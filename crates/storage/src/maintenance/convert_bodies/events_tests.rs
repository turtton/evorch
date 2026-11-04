//! The events step of the conversion: what a version 33, 40, 41 or 42 database's events
//! become, and what stops it.

use event_bus::{CompactionEvent, EventKind, MessageEvent, ToolEvent};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::tests::{
    apply, database_at, files, insert, long, plain_run, preview, seeded, seeded_at,
    seeded_with_maps, version,
};
use super::*;
use crate::body_store::PRODUCTION_OWNERS;
use crate::migrations::MIGRATIONS;
use crate::repo::content_blobs::{BodyScope, check_bodies};
use crate::repo::event::{kind_name, list_all_ordered};

#[allow(clippy::cast_possible_truncation)]
const LATEST: u32 = MIGRATIONS.len() as u32;

/// An event as the writer before this format stored it: plain JSON, compressed when large.
fn insert_event(conn: &Connection, session: Option<&str>, kind: &EventKind, at: i64) -> i64 {
    let json = serde_json::to_string(kind).unwrap();
    insert_raw(
        conn,
        session,
        kind_name(kind),
        Some(kind.subkind_name()),
        &json,
        kind.primary_run_id(),
        at,
    )
}

fn insert_raw(
    conn: &Connection,
    session: Option<&str>,
    kind: &str,
    subkind: Option<&str>,
    json: &str,
    run_id: Option<&str>,
    at: i64,
) -> i64 {
    let stored = crate::repo::event::encode_payload(kind, subkind.unwrap_or(""), json);
    conn.execute(
        "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
         payload, subkind, run_id) VALUES (?1, 1, ?2, ?2, ?3, ?4, ?5, ?6)",
        params![session, at, kind, stored, subkind, run_id],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn message(text: String) -> EventKind {
    MessageEvent::MessageCompleted {
        run_id: "run-b-small".into(),
        text,
    }
    .into()
}

fn tool_completed(output: String, detail: Option<Value>) -> EventKind {
    ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "call".into(),
        is_error: false,
        output: Some(output),
        detail,
        run_id: Some("run-big".into()),
        parent_call_id: None,
    }
    .into()
}

/// What `seed_events` stored: every event with the JSON it must read back as, and whether
/// the step converts it.
struct Seeded {
    events: Vec<(i64, EventKind, bool)>,
    /// Ids of rows this build cannot read (a variant that no longer exists).
    unreadable: Vec<i64>,
}

fn seed_events(conn: &Connection) -> Seeded {
    conn.execute(
        "INSERT INTO sessions(id, status, created_at_ns, updated_at_ns) VALUES ('s', 'running', 0, 0)",
        [],
    )
    .unwrap();
    let shared = long("shared tool output", 5_000);
    let compaction: EventKind = CompactionEvent::Compacted {
        operation_id: "fixture-compaction".into(),
        run_id: "run-big".into(),
        reason: event_bus::CompactionReason::Automatic,
        threshold: 0.8,
        context_window_tokens: 1000,
        window_source: Default::default(),
        estimated_tokens_before: 900,
        estimated_tokens_after: 100,
        compacted_range_start: 0,
        compacted_range_end: 4,
        checkpoint_id: "cp".into(),
        summary: long("summary", 4_000),
    }
    .into();
    let approval: EventKind = ToolEvent::ApprovalRequested {
        tool_name: "write".into(),
        call_id: "run-big:approve".into(),
        input: Some(json!({"content": long("approval", 6_000)})),
    }
    .into();
    let rows: Vec<(EventKind, bool)> = vec![
        // The run's own shared body: the event must find it already stored.
        (message(shared.clone()), true),
        (tool_completed(shared, None), true),
        (
            tool_completed(
                "short".into(),
                Some(
                    json!({"files": [long("detail", 3_000), "tail"], "nested": {"deep": [long("deep", 2_100)]}}),
                ),
            ),
            true,
        ),
        (compaction, true),
        (message("x".repeat(2_047)), false),
        (message("x".repeat(2_048)), true),
        (message("\u{e000}looks like a token".into()), true),
        (message("small".into()), false),
        (approval, false),
        (
            ToolEvent::ToolStarted {
                tool_name: "shell".into(),
                call_id: "call".into(),
                input: Some(json!({"cmd": long("input", 2_500)})),
                run_id: Some("run-big".into()),
                parent_call_id: None,
            }
            .into(),
            true,
        ),
    ];
    let mut events = Vec::new();
    for (index, (kind, converts)) in rows.into_iter().enumerate() {
        let id = insert_event(conn, Some("s"), &kind, 10 + index as i64);
        events.push((id, kind, converts));
    }
    // A variant this build no longer has: the step still converts it as plain JSON.
    let removed = json!({"kind": "Message", "payload": {"kind": "RemovedVariant", "payload": {
        "run_id": "run-big", "text": long("removed", 3_000), "extra": [1, 2.5, null, true]}}});
    let removed_id = insert_raw(
        conn,
        Some("s"),
        "Message",
        Some("RemovedVariant"),
        &removed.to_string(),
        Some("run-big"),
        100,
    );
    // The totals of a database written by the previous format agree with its events.
    conn.execute(
        "UPDATE sessions SET total_event_bytes = \
         (SELECT SUM(OCTET_LENGTH(payload)) FROM events WHERE session_id = 's')",
        [],
    )
    .unwrap();
    Seeded {
        events,
        unreadable: vec![removed_id],
    }
}

fn query<T: rusqlite::types::FromSql>(conn: &Connection, sql: &str) -> T {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn assert_accounting_matches_rows(conn: &Connection) {
    assert_eq!(
        query::<i64>(
            conn,
            "SELECT COALESCE(SUM(payload_bytes), 0) FROM event_session_bytes"
        ),
        query::<i64>(
            conn,
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload) + blob_bytes), 0) FROM events \
             WHERE session_id IS NOT NULL"
        )
    );
    assert_eq!(
        query::<i64>(
            conn,
            "SELECT COALESCE(SUM(payload_bytes), 0) FROM event_day_bytes"
        ),
        query::<i64>(
            conn,
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload) + blob_bytes), 0) FROM events"
        )
    );
    assert_eq!(
        query::<i64>(
            conn,
            "SELECT total_event_bytes FROM sessions WHERE id = 's'"
        ),
        query::<i64>(
            conn,
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload) + blob_bytes), 0) FROM events \
             WHERE session_id = 's'"
        )
    );
}

fn assert_events_read_back(conn: &Connection, seeded: &Seeded) {
    let read = list_all_ordered(conn).unwrap();
    let expected: Vec<_> = seeded
        .events
        .iter()
        .map(|(id, kind, _)| (*id, kind.clone()))
        .collect();
    let actual: Vec<_> = read
        .into_iter()
        .map(|stored| (stored.id, stored.event.kind))
        .collect();
    assert_eq!(
        actual, expected,
        "every reader returns the events it returned before"
    );
}

#[test]
fn a_version_33_database_gets_runs_and_events_converted_by_the_one_command() {
    // Given: a version 33 database with runs and events whose bodies the runs share.
    let (_dir, path, _) = seeded_at(33);
    let conn = Connection::open(&path).unwrap();
    let seeded_events = seed_events(&conn);
    drop(conn);

    // When: it is previewed, then converted.
    let before_preview = files(&path);
    let preview = preview(&path).unwrap();
    assert_eq!(files(&path), before_preview, "a preview writes nothing");
    let analysis = preview.analysis.as_ref().unwrap();
    let to_convert = seeded_events.events.iter().filter(|(_, _, c)| *c).count() as u64 + 1;
    assert_eq!(analysis.events.eligible_events, to_convert);
    assert_eq!(analysis.events.unconvertible_events, 0);
    let summary = apply(&path).unwrap();

    // Then: one apply did everything, nothing mismatched, nothing is left unconverted.
    assert_eq!((summary.source_schema, summary.target_schema), (33, LATEST));
    assert_eq!(summary.events.converted_events, to_convert);
    assert_eq!(summary.events_tokenized, to_convert);
    assert_eq!(summary.events.unconverted_remaining, 0);
    assert_eq!(summary.events.mismatched_events, 0);
    assert!(summary.events.batches >= 1);
    assert_eq!(version(&path), LATEST);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        query::<u64>(&conn, "SELECT COUNT(*) FROM events WHERE body_format = 1"),
        to_convert
    );
    // Events that are not targets stay as they were.
    for (id, _, converts) in &seeded_events.events {
        let format: i64 = conn
            .query_row("SELECT body_format FROM events WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(format == 1, *converts, "event {id}");
    }
    assert_events_read_back(&conn, &seeded_events);
    assert_accounting_matches_rows(&conn);
    assert!(
        check_bodies(&conn, PRODUCTION_OWNERS, BodyScope::All, true)
            .unwrap()
            .is_ok()
    );
    // The body a run created first costs the event that names it nothing.
    let shared_event = seeded_events.events[0].0;
    assert_eq!(
        query::<i64>(
            &conn,
            &format!("SELECT blob_bytes FROM events WHERE id = {shared_event}")
        ),
        0
    );
    assert!(query::<i64>(&conn, "SELECT SUM(blob_bytes) FROM events") > 0);
    // The removed variant is converted as plain JSON and resolves to what it was.
    let removed = seeded_events.unreadable[0];
    let (format, payload): (i64, rusqlite::types::Value) = conn
        .query_row(
            "SELECT body_format, payload FROM events WHERE id = ?1",
            [removed],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(format, 1);
    let resolved = crate::repo::event::resolved_payload_json(
        &conn,
        rusqlite::types::ValueRef::from(&payload),
        crate::repo::event::StoredFormat::Tokenized,
        "test",
    )
    .unwrap();
    let value: Value = serde_json::from_str(&resolved).unwrap();
    assert_eq!(
        value["payload"]["payload"]["extra"],
        json!([1, 2.5, null, true])
    );
    assert_eq!(
        value["payload"]["payload"]["text"].as_str().unwrap(),
        long("removed", 3_000)
    );
}

#[test]
fn a_version_42_database_converts_only_its_events_and_leaves_every_run_alone() {
    // Given: a version 42 database (runs in rows, events plain) with events.
    let (_dir, path, _) = seeded();
    apply(&path).unwrap();
    crate::maintenance::downgrade_to_v42(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    let seeded_events = seed_events(&conn);
    let runs_before: Vec<(i64, String, Vec<u8>, i64)> = conn
        .prepare("SELECT id, run_id, msg_sha256, ordinal FROM run_messages ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let revisions_before: Vec<(String, i64)> = conn
        .prepare("SELECT run_id, header_rev FROM run_contexts ORDER BY run_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    drop(conn);
    assert_eq!(version(&path), 42);

    // When: it is converted.
    let summary = apply(&path).unwrap();

    // Then: events are converted, no run was converted or touched.
    assert_eq!(summary.converted_runs, 0);
    assert_eq!(summary.migrations_applied, 0);
    assert_eq!((summary.source_schema, summary.target_schema), (42, LATEST));
    assert!(summary.events.converted_events > 0);
    assert_eq!(summary.events.mismatched_events, 0);
    let conn = Connection::open(&path).unwrap();
    let runs_after: Vec<(i64, String, Vec<u8>, i64)> = conn
        .prepare("SELECT id, run_id, msg_sha256, ordinal FROM run_messages ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let revisions_after: Vec<(String, i64)> = conn
        .prepare("SELECT run_id, header_rev FROM run_contexts ORDER BY run_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        runs_after, runs_before,
        "message ids, hashes and ordinals are unchanged"
    );
    assert_eq!(revisions_after, revisions_before, "no header was written");
    assert_events_read_back(&conn, &seeded_events);
    assert_accounting_matches_rows(&conn);
}

#[test]
fn a_version_40_database_converts_its_maps_and_its_events_without_touching_a_run() {
    // Given: a version 40 database (runs in rows, maps in one column, events plain).
    let (_dir, path, sidecars) = seeded_with_maps();
    apply(&path).unwrap();
    crate::maintenance::downgrade_to_v40(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    let seeded_events = seed_events(&conn);
    let runs_before: Vec<(i64, String, Vec<u8>, i64)> = conn
        .prepare("SELECT id, run_id, msg_sha256, ordinal FROM run_messages ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    drop(conn);
    assert_eq!(version(&path), 40);

    // When: it is converted.
    let summary = apply(&path).unwrap();

    // Then: the maps and the events are converted in one command; no run is.
    assert_eq!(summary.converted_runs, 0);
    assert_eq!((summary.source_schema, summary.target_schema), (40, LATEST));
    assert_eq!(summary.converted_provenance_maps, sidecars.len() as u64);
    assert!(summary.events.converted_events > 0);
    assert_eq!(summary.events.mismatched_events, 0);
    assert_eq!(summary.events.unconverted_remaining, 0);
    assert_eq!(version(&path), LATEST);
    let conn = Connection::open(&path).unwrap();
    let runs_after: Vec<(i64, String, Vec<u8>, i64)> = conn
        .prepare("SELECT id, run_id, msg_sha256, ordinal FROM run_messages ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(runs_after, runs_before);
    assert_events_read_back(&conn, &seeded_events);
    assert_accounting_matches_rows(&conn);
}

#[test]
fn a_converted_database_is_a_read_only_no_op_for_events_too() {
    let (_dir, path, _) = seeded();
    apply(&path).unwrap();
    let before = files(&path);
    let again = apply(&path).unwrap();
    assert!(again.already_converted);
    assert_eq!(files(&path), before, "no backup, no write, no VACUUM");
}

#[test]
fn a_half_prepared_version_42_database_is_refused_unchanged() {
    // Given: version 42 with the event columns already added (a conversion that stopped).
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), 42);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("ALTER TABLE events ADD COLUMN body_format INTEGER NOT NULL DEFAULT 0")
        .unwrap();
    drop(conn);
    let before = files(&path);

    // When/Then: both modes refuse it and point at the backup.
    for run in [preview(&path), apply(&path)] {
        let failure = run.unwrap_err();
        assert!(failure.detail.contains("did not finish"), "{failure}");
    }
    assert_eq!(files(&path), before);
}

#[test]
fn events_that_cannot_be_read_stop_apply_before_any_change() {
    // Given: a version 42 database with an event whose payload is not JSON.
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), 42);
    let conn = Connection::open(&path).unwrap();
    insert_raw(
        &conn,
        None,
        "Message",
        Some("MessageCompleted"),
        &format!("{{\"broken\": \"{}", "x".repeat(3_000)),
        None,
        1,
    );
    drop(conn);
    let before = files(&path);

    // When: it is previewed and applied.
    let preview = preview(&path).unwrap();
    let analysis = preview.analysis.unwrap();
    assert_eq!(analysis.events.unconvertible_events, 1);
    assert_eq!(analysis.events.problems.len(), 1);
    let failure = apply(&path).unwrap_err();

    // Then: apply refused on the analysis, before the backup, with nothing changed.
    assert!(
        failure.detail.contains("events cannot be converted"),
        "{failure}"
    );
    assert!(failure.backup.is_none() && !failure.restore_required);
    assert_eq!(files(&path), before);
}

#[test]
fn a_payload_of_exactly_the_batch_limit_converts_and_one_byte_more_is_refused() {
    for (extra, converts) in [(0_u64, true), (1, false)] {
        // Given: a version 42 database with one event of a chosen decoded size.
        let dir = tempfile::tempdir().unwrap();
        let path = database_at(dir.path(), 42);
        let conn = Connection::open(&path).unwrap();
        let shell = |text: &str| {
            json!({"kind": "Message", "payload": {"kind": "MessageCompleted",
                "payload": {"run_id": "r", "text": text}}})
            .to_string()
        };
        let limit = shell(&"a".repeat(3_000)).len() as u64;
        insert_raw(
            &conn,
            None,
            "Message",
            Some("MessageCompleted"),
            &shell(&"a".repeat(3_000 + extra as usize)),
            Some("r"),
            1,
        );
        drop(conn);
        events::limit_override::set(Some(limit));

        // When: it is applied with the limit set to the event's size.
        let result = apply(&path);
        events::limit_override::set(None);

        // Then: the event of exactly the limit converts; one byte more is refused up front.
        if converts {
            assert_eq!(result.unwrap().events.converted_events, 1);
        } else {
            let failure = result.unwrap_err();
            assert!(failure.detail.contains("above"), "{failure}");
            assert!(!failure.restore_required);
        }
    }
}

#[test]
fn a_batch_never_holds_more_payload_than_the_limit_and_rows_limit_the_count() {
    // Given: five events of about 3 KiB each, and a limit that fits two of them.
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), 42);
    let conn = Connection::open(&path).unwrap();
    let kinds: Vec<EventKind> = (0..5)
        .map(|n| message(long(&format!("body {n}"), 3_000)))
        .collect();
    for (n, kind) in kinds.iter().enumerate() {
        insert_event(&conn, None, kind, n as i64);
    }
    let one = serde_json::to_string(&kinds[0]).unwrap().len() as u64;
    drop(conn);
    events::limit_override::set(Some(one * 2 + one / 2));

    // When: converted with the default rows per batch.
    let summary = apply(&path);
    events::limit_override::set(None);
    let summary = summary.unwrap();

    // Then: three batches of 2, 2 and 1, none above the limit.
    assert_eq!(summary.events.converted_events, 5);
    assert_eq!(summary.events.batches, 3);
    assert!(summary.events.max_batch_expanded_bytes <= one * 2 + one / 2);

    // And: with one row per batch every event is its own transaction.
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), 42);
    let conn = Connection::open(&path).unwrap();
    for (n, kind) in kinds.iter().enumerate() {
        insert_event(&conn, None, kind, n as i64);
    }
    drop(conn);
    let summary = convert_bodies(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            event_batch_rows: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(summary.events.batches, 5);
    assert_eq!(summary.events.batch_rows_limit, 1);
}

#[test]
fn a_batch_wal_stays_within_the_stated_bound_whatever_the_event_count() {
    // Given: events with bodies of several sizes, converted one per transaction.
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), 42);
    let conn = Connection::open(&path).unwrap();
    for (n, size) in [3_000, 40_000, 400_000].into_iter().enumerate() {
        insert_event(
            &conn,
            None,
            &message(long(&format!("w{n}"), size)),
            n as i64,
        );
    }
    let page: u64 = query(&conn, "PRAGMA page_size");
    drop(conn);
    let summary = convert_bodies(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            event_batch_rows: Some(1),
            ..Default::default()
        },
    )
    .unwrap();

    // Then: the largest batch fits frames <= ceil(2 S / page) + 128 + 16 (E + R + B).
    let s = summary.events.payload_stored_bytes_before
        + summary.events.payload_stored_bytes_after
        + summary.events.blob_stored_bytes_created;
    let bound = (2 * s).div_ceil(page)
        + 128
        + 16 * (1 + summary.events.references_created + summary.events.blobs_created);
    assert!(
        summary.events.peak_batch_wal_frames <= bound,
        "{} frames, bound {bound}",
        summary.events.peak_batch_wal_frames
    );
    assert!(summary.events.peak_batch_wal_frames > 0);
}

#[test]
fn a_stop_in_the_events_step_is_recovered_from_the_backup_and_the_next_run_converts() {
    for point in [
        Point::AfterEventsPrepare,
        Point::Event(EventPoint::BeforeReadback { id: 0 }),
        Point::Event(EventPoint::BeforeCommit),
        Point::AfterEventBatch { frames: 0 },
    ] {
        // Given: a version 33 database stopped at a point of the events step.
        let (_dir, path, _) = seeded_at(33);
        let conn = Connection::open(&path).unwrap();
        let seeded_events = seed_events(&conn);
        drop(conn);
        let original = std::fs::read(&path).unwrap();
        let failure = convert_with(
            &path,
            &ConvertBodiesOptions {
                apply: true,
                event_batch_rows: Some(2),
                ..Default::default()
            },
            &mut |at, _| {
                let same = match (at, point) {
                    (
                        Point::Event(EventPoint::BeforeReadback { .. }),
                        Point::Event(EventPoint::BeforeReadback { .. }),
                    ) => true,
                    _ => {
                        std::mem::discriminant(&at) == std::mem::discriminant(&point)
                            && matches!(at, Point::Event(_)) == matches!(point, Point::Event(_))
                            && (!matches!(at, Point::Event(_)) || at == point)
                    }
                };
                if same {
                    return Err(StorageError::Serialization("injected".into()));
                }
                Ok(())
            },
        )
        .unwrap_err();

        // Then: the report says where and keeps the backup; a normal open refuses the file.
        assert!(failure.restore_required, "{point:?}");
        assert!(failure.detail.contains("injected"));
        let backup = failure.backup.clone().unwrap();
        assert!(
            crate::Database::open(&crate::StorageConfig {
                db_path: path.clone(),
                ..Default::default()
            })
            .is_err()
        );

        // When: the backup is restored and the command runs again from the start.
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(crate::db::suffixed_path(&path, suffix));
        }
        std::fs::copy(&backup, &path).unwrap();
        assert_eq!(version(&path), 33, "the backup is the source version");
        assert!(!original.is_empty());
        let summary = convert_bodies(
            &path,
            &ConvertBodiesOptions {
                apply: true,
                backup: Some(path.with_extension("second.db")),
                ..Default::default()
            },
        )
        .unwrap();

        // Then: it succeeds and every event reads back.
        assert_eq!(summary.events.mismatched_events, 0);
        assert_events_read_back(&Connection::open(&path).unwrap(), &seeded_events);
    }
}

#[test]
fn a_run_and_an_event_share_one_blob_and_the_collector_keeps_it() {
    // Given: a body that a run's message and an event both hold.
    let (_dir, path, _) = seeded_at(33);
    let conn = Connection::open(&path).unwrap();
    let shared = long("shared tool output", 5_000);
    insert_event(&conn, None, &message(shared.clone()), 1);
    drop(conn);

    // When: converted.
    apply(&path).unwrap();

    // Then: one blob row serves the message and the event, and survives a collection after
    // the event is deleted.
    let conn = Connection::open(&path).unwrap();
    let token = crate::body_ref::BodyToken::of(&shared);
    let (blob, messages, events): (i64, i64, i64) = conn
        .query_row(
            "SELECT b.id, (SELECT COUNT(*) FROM content_blob_refs_message WHERE blob_id = b.id), \
             (SELECT COUNT(*) FROM content_blob_refs_event WHERE blob_id = b.id) \
             FROM content_blobs b WHERE b.hash = ?1",
            [&token.hash[..]],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(messages > 0 && events == 1, "blob {blob}");
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn.execute("DELETE FROM events", []).unwrap();
    crate::maintenance::compact::collect_unreferenced_blobs(&conn).unwrap();
    assert_eq!(
        query::<i64>(
            &conn,
            &format!("SELECT COUNT(*) FROM content_blobs WHERE id = {blob}")
        ),
        1
    );
}

#[test]
fn free_space_one_byte_short_refuses_before_the_backup_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), 42);
    let conn = Connection::open(&path).unwrap();
    insert_event(&conn, None, &message(long("space", 50_000)), 1);
    drop(conn);
    let before = files(&path);
    let required = preview(&path).unwrap().required_free_space_bytes;
    assert!(required > 0);
    space::tests_override::set(Some(required - 1));
    let failure = apply(&path).unwrap_err();
    space::tests_override::set(None);
    assert!(
        failure.backup.is_none() && !failure.restore_required,
        "{failure}"
    );
    assert_eq!(files(&path), before);
}

#[test]
fn the_plain_runs_helper_is_used() {
    // Keeps the shared helper imports honest for the tests above.
    let _ = (
        plain_run("x", &json!([]), 1),
        insert as fn(&Connection, &super::tests::Run),
    );
}
