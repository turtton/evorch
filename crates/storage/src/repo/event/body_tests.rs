//! Event payloads whose long strings are tokens: what is stored, what every reader sees,
//! and what a failed append leaves behind.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use event_bus::{
    CompactionEvent, Event, EventKind, EventMeta, LifecycleEvent, MessageEvent, ToolEvent,
};
use rusqlite::hooks::{AuthAction, Authorization};
use rusqlite::{Connection, params};
use serde_json::json;

use super::deferred::{
    BodyFailureClass, BodyRef, DeferredRow, MAX_RESOLVE_REFS, StoredEventFailureReason,
    for_each_event_ordered_deferred, resolve_bodies,
};
use super::{EventAccounting, StoredFormat, append_event, list_all_ordered, list_by_session};
use crate::body_ref::BodyToken;
use crate::body_store::{PRODUCTION_OWNERS, put_blob};
use crate::inspect::{EventScope, InspectBodies, InspectEventsOptions};
use crate::repo::content_blobs::{BodyScope, check_bodies};
use crate::{Database, HardLimits, LimitKind, StorageError};

fn event_at(kind: impl Into<EventKind>, nanos: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_nanos(nanos),
            wall_clock: UNIX_EPOCH + Duration::from_nanos(nanos),
        },
        kind: kind.into(),
    }
}

/// Text that does not compress, so that a blob's stored size follows its length.
fn noise(seed: u64, bytes: usize) -> String {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut text = String::with_capacity(bytes);
    while text.len() < bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        text.push_str(&format!("{state:016x}"));
    }
    text.truncate(bytes);
    text
}

fn completed(text: &str) -> EventKind {
    MessageEvent::MessageCompleted {
        run_id: "run-a".into(),
        text: text.into(),
    }
    .into()
}

fn tool_completed(output: &str, detail: Option<serde_json::Value>) -> EventKind {
    ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "call".into(),
        is_error: false,
        output: Some(output.into()),
        detail,
        run_id: Some("run-a".into()),
        parent_call_id: None,
    }
    .into()
}

fn database() -> Database {
    let database = Database::open_in_memory().unwrap();
    database
        .conn
        .execute(
            "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) \
             VALUES ('s1', 'running', 0, 0)",
            [],
        )
        .unwrap();
    database
}

fn append(database: &Database, accounting: &mut EventAccounting, kind: EventKind, n: u64) -> i64 {
    append_event(
        &database.conn,
        Some("s1"),
        &event_at(kind, n),
        &HardLimits::default(),
        accounting,
    )
    .unwrap()
    .id
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn format_of(conn: &Connection, id: i64) -> i64 {
    conn.query_row(
        "SELECT body_format FROM events WHERE id = ?1",
        [id],
        |row| row.get(0),
    )
    .unwrap()
}

fn assert_accounting(conn: &Connection) {
    assert_eq!(
        count(
            conn,
            "SELECT COALESCE(SUM(payload_bytes), 0) FROM event_session_bytes"
        ),
        count(
            conn,
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload) + blob_bytes), 0) FROM events \
             WHERE session_id IS NOT NULL"
        )
    );
    assert_eq!(
        count(
            conn,
            "SELECT COALESCE(SUM(payload_bytes), 0) FROM event_day_bytes"
        ),
        count(
            conn,
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload) + blob_bytes), 0) FROM events"
        )
    );
    assert_eq!(
        count(
            conn,
            "SELECT total_event_bytes FROM sessions WHERE id = 's1'"
        ),
        count(
            conn,
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload) + blob_bytes), 0) FROM events \
             WHERE session_id = 's1'"
        )
    );
}

#[test]
fn every_targeted_kind_round_trips_through_tokens() {
    // Given: one event of each kind that carries a long string.
    let database = database();
    let mut accounting = EventAccounting::default();
    let body = noise(1, 5_000);
    let kinds: Vec<EventKind> = vec![
        completed(&body),
        MessageEvent::ReasoningDelta {
            delta: noise(2, 3_000),
            run_id: Some("run-a".into()),
        }
        .into(),
        MessageEvent::MessagePart {
            run_id: "run-a".into(),
            response_id: "r".into(),
            text: noise(3, 4_000),
            index: 0,
            total: 1,
        }
        .into(),
        ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "c".into(),
            input: Some(json!({"cmd": noise(4, 2_500), "nested": [{"deep": noise(5, 2_100)}]})),
            run_id: Some("run-a".into()),
            parent_call_id: None,
        }
        .into(),
        tool_completed(
            &noise(6, 3_000),
            Some(json!({"files": [noise(7, 2_048), "x"]})),
        ),
        CompactionEvent::Compacted {
            operation_id: "fixture-compaction".into(),
            run_id: "run-a".into(),
            reason: event_bus::CompactionReason::Automatic,
            threshold: 0.8,
            context_window_tokens: 1000,
            window_source: Default::default(),
            estimated_tokens_before: 900,
            estimated_tokens_after: 100,
            compacted_range_start: 0,
            compacted_range_end: 4,
            checkpoint_id: "cp".into(),
            summary: noise(8, 4_000),
        }
        .into(),
    ];

    // When: they are appended and read back by every reader.
    let ids: Vec<i64> = kinds
        .iter()
        .enumerate()
        .map(|(n, kind)| append(&database, &mut accounting, kind.clone(), n as u64 + 1))
        .collect();

    // Then: each is stored as a token container, and readers return the original events.
    for id in &ids {
        assert_eq!(format_of(&database.conn, *id), 1, "event {id}");
        assert!(
            count(
                &database.conn,
                &format!("SELECT OCTET_LENGTH(payload) FROM events WHERE id = {id}")
            ) < 1_500
        );
    }
    let read = list_by_session(&database.conn, "s1").unwrap();
    assert_eq!(
        read.iter()
            .map(|stored| stored.event.kind.clone())
            .collect::<Vec<_>>(),
        kinds
    );
    assert_eq!(list_all_ordered(&database.conn).unwrap(), read);
    assert!(
        check_bodies(&database.conn, PRODUCTION_OWNERS, BodyScope::All, true)
            .unwrap()
            .is_ok()
    );
    assert_accounting(&database.conn);
}

#[test]
fn the_token_threshold_and_the_escape_character_decide_what_is_tokenized() {
    let database = database();
    let mut accounting = EventAccounting::default();
    let token_like = BodyToken::of("some other text").to_value_string();
    let cases: Vec<(String, i64)> = vec![
        ("x".repeat(2_047), 0),
        ("x".repeat(2_048), 1),
        // UTF-8 length, not characters: 683 three-byte characters are 2,049 bytes.
        ("あ".repeat(682), 0),
        ("あ".repeat(683), 1),
        ("\u{e000}short".into(), 1),
        (token_like.clone(), 1),
        (token_like.replace('\u{e000}', ""), 0),
        ("evorch-body:v1:text without the escape".into(), 0),
    ];
    for (n, (text, expected)) in cases.iter().enumerate() {
        let id = append(&database, &mut accounting, completed(text), n as u64 + 1);
        assert_eq!(format_of(&database.conn, id), *expected, "case {n}");
    }
    let read = list_all_ordered(&database.conn).unwrap();
    for ((text, _), stored) in cases.iter().zip(&read) {
        let EventKind::Message(MessageEvent::MessageCompleted { text: got, .. }) =
            &stored.event.kind
        else {
            panic!("not a completed message");
        };
        assert_eq!(got, text, "a string that looks like a token is only text");
    }
}

#[test]
fn kinds_that_sql_reads_inside_stay_inline_whatever_their_size() {
    let database = database();
    let mut accounting = EventAccounting::default();
    let approval: EventKind = ToolEvent::ApprovalRequested {
        correlation: None,
        tool_name: "write".into(),
        call_id: "run-a:call".into(),
        input: Some(json!({"content": noise(9, 6_000)})),
    }
    .into();
    let id = append(&database, &mut accounting, approval, 1);
    assert_eq!(format_of(&database.conn, id), 0);
    assert_eq!(
        count(&database.conn, "SELECT COUNT(*) FROM content_blobs"),
        0
    );
}

#[test]
fn redaction_secret_guard_and_the_plain_size_limit_work_before_tokenizing() {
    let database = database();
    let mut accounting = EventAccounting::default();
    let before = accounting.clone();

    // A secret inside a long body is refused, and nothing is stored.
    let secret = format!(
        "{} sk-test-evorch-9f8e7d6c5b4a3f2e1d {}",
        noise(1, 3_000),
        noise(2, 3_000)
    );
    let error = append_event(
        &database.conn,
        Some("s1"),
        &event_at(completed(&secret), 1),
        &HardLimits::default(),
        &mut accounting,
    )
    .unwrap_err();
    assert!(
        matches!(error, StorageError::SecretDetected { .. }),
        "{error:?}"
    );

    // The size limit is the plain size: a body the token container would fit is refused.
    let limits = HardLimits {
        max_event_bytes: 4_000,
        ..HardLimits::default()
    };
    let error = append_event(
        &database.conn,
        Some("s1"),
        &event_at(completed(&noise(3, 5_000)), 2),
        &limits,
        &mut accounting,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StorageError::LimitExceeded {
            limit: LimitKind::EventSize,
            ..
        }
    ));
    for table in ["events", "content_blobs", "content_blob_refs_event"] {
        assert_eq!(
            count(&database.conn, &format!("SELECT COUNT(*) FROM {table}")),
            0
        );
    }
    assert_eq!(accounting, before);
}

#[test]
fn a_quota_failure_leaves_no_blob_reference_event_or_accounting_change() {
    let database = database();
    let mut accounting = EventAccounting::default();
    append(&database, &mut accounting, completed("hello"), 1);
    let before = accounting.clone();
    let rows = |table: &str| count(&database.conn, &format!("SELECT COUNT(*) FROM {table}"));
    let counts_before = (
        rows("events"),
        rows("content_blobs"),
        rows("content_blob_refs_event"),
    );
    let session_before = count(&database.conn, "SELECT total_event_bytes FROM sessions");

    // When: the new blob's stored bytes alone exceed what the session may hold.
    let limits = HardLimits {
        max_session_bytes: 2_000,
        ..HardLimits::default()
    };
    let error = append_event(
        &database.conn,
        Some("s1"),
        &event_at(completed(&noise(5, 20_000)), 2),
        &limits,
        &mut accounting,
    )
    .unwrap_err();

    // Then: the blob went with everything else.
    assert!(matches!(
        error,
        StorageError::LimitExceeded {
            limit: LimitKind::SessionSize,
            ..
        }
    ));
    assert_eq!(
        (
            rows("events"),
            rows("content_blobs"),
            rows("content_blob_refs_event")
        ),
        counts_before
    );
    assert_eq!(accounting, before);
    assert_eq!(
        count(&database.conn, "SELECT total_event_bytes FROM sessions"),
        session_before
    );
    assert_accounting(&database.conn);
}

#[test]
fn blob_bytes_charge_only_the_blobs_an_event_created() {
    let database = database();
    let mut accounting = EventAccounting::default();
    let a = noise(10, 6_000);
    let b = noise(11, 7_000);
    let c = noise(12, 3_000);

    // A body a run stored first.
    database.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    put_blob(&database.conn, &c).unwrap();
    database.conn.execute_batch("COMMIT").unwrap();

    let e1 = append(&database, &mut accounting, completed(&a), 1);
    let e2 = append(&database, &mut accounting, completed(&a), 2);
    // One event holding the same new body twice, and the old body.
    let e3 = append(
        &database,
        &mut accounting,
        tool_completed(&b, Some(json!({"again": b, "old": c}))),
        3,
    );
    let blob_bytes = |id: i64| {
        count(
            &database.conn,
            &format!("SELECT blob_bytes FROM events WHERE id = {id}"),
        )
    };
    let stored = |text: &str| {
        let token = BodyToken::of(text);
        database
            .conn
            .query_row(
                "SELECT OCTET_LENGTH(data) FROM content_blobs WHERE hash = ?1",
                [&token.hash[..]],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(blob_bytes(e1), stored(&a));
    assert_eq!(
        blob_bytes(e2),
        0,
        "a body another event created costs nothing"
    );
    assert_eq!(
        blob_bytes(e3),
        stored(&b),
        "a repeated body is charged once, the old one not"
    );
    for (id, refs) in [(e1, 1), (e2, 1), (e3, 2)] {
        assert_eq!(
            count(
                &database.conn,
                &format!("SELECT COUNT(*) FROM content_blob_refs_event WHERE event_id = {id}")
            ),
            refs,
            "event {id}"
        );
    }
    assert_accounting(&database.conn);

    // Deleting the creator moves no charge: the other owner keeps its zero.
    database
        .conn
        .execute("DELETE FROM events WHERE id = ?1", [e1])
        .unwrap();
    assert_eq!(blob_bytes(e2), 0);
    assert_accounting(&database.conn);
    // And the shared blob stays referenced.
    assert_eq!(
        count(
            &database.conn,
            "SELECT COUNT(*) FROM content_blob_refs_event r JOIN content_blobs b ON b.id = r.blob_id"
        ),
        3
    );
}

#[test]
fn the_accounting_triggers_follow_insert_update_delete_and_seed_a_late_session() {
    let database = database();
    let mut accounting = EventAccounting::default();
    let first = append(&database, &mut accounting, completed(&noise(20, 4_000)), 1);
    append(&database, &mut accounting, completed("small"), 2);
    assert_accounting(&database.conn);

    database
        .conn
        .execute(
            "UPDATE events SET payload = ?2 WHERE id = ?1",
            params![first, "{\"kind\":\"Message\",\"payload\":{\"kind\":\"MessageCompleted\",\"payload\":{\"run_id\":\"run-a\",\"text\":\"short\"}}}"],
        )
        .unwrap();
    assert_accounting(&database.conn);
    database
        .conn
        .execute("UPDATE events SET blob_bytes = blob_bytes + 100", [])
        .unwrap();
    assert_accounting(&database.conn);
    database
        .conn
        .execute("DELETE FROM events WHERE id = ?1", [first])
        .unwrap();
    assert_accounting(&database.conn);

    // A session created after its events is seeded with their cost, blobs included.
    let mut accounting = EventAccounting::default();
    append_event(
        &database.conn,
        Some("s9"),
        &event_at(
            LifecycleEvent::Started {
                session_id: "s9".into(),
            },
            5,
        ),
        &HardLimits::default(),
        &mut accounting,
    )
    .unwrap();
    append_event(
        &database.conn,
        Some("s9"),
        &event_at(completed(&noise(21, 5_000)), 6),
        &HardLimits::default(),
        &mut accounting,
    )
    .unwrap();
    database
        .conn
        .execute("DELETE FROM sessions WHERE id = 's9'", [])
        .unwrap();
    crate::projection::reconcile(&database.conn).unwrap();
    assert_eq!(
        count(
            &database.conn,
            "SELECT total_event_bytes FROM sessions WHERE id = 's9'"
        ),
        count(
            &database.conn,
            "SELECT SUM(OCTET_LENGTH(payload) + blob_bytes) FROM events WHERE session_id = 's9'"
        )
    );
    assert_eq!(
        count(
            &database.conn,
            "SELECT payload_bytes FROM event_session_bytes WHERE session_id = 's9'"
        ),
        count(
            &database.conn,
            "SELECT total_event_bytes FROM sessions WHERE id = 's9'"
        )
    );
}

/// Removes the blob a token names, behind its references' back.
fn damage_blob(conn: &Connection, text: &str, how: &str) {
    let token = BodyToken::of(text);
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    match how {
        "missing" => {
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM content_blobs WHERE hash = ?1",
                    [&token.hash[..]],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "DELETE FROM content_blob_refs_event WHERE blob_id = ?1",
                [id],
            )
            .unwrap();
            conn.execute("DELETE FROM content_blob_gc_queue", [])
                .unwrap();
            conn.execute("DELETE FROM content_blobs WHERE id = ?1", [id])
                .unwrap();
        }
        _ => {
            conn.execute(
                "UPDATE content_blobs SET codec = 0, data = CAST('garbage' AS BLOB) WHERE hash = ?1",
                [&token.hash[..]],
            )
            .unwrap();
        }
    }
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
}

#[test]
fn a_body_that_cannot_be_resolved_fails_every_normal_reader_and_not_the_deferred_ones() {
    for how in ["missing", "corrupt"] {
        // Given: an event whose blob is gone (or unreadable), next to a healthy one.
        let database = database();
        let mut accounting = EventAccounting::default();
        let lost = noise(30, 4_000);
        let lost_id = append(&database, &mut accounting, completed(&lost), 1);
        let kept = noise(31, 4_000);
        append(&database, &mut accounting, completed(&kept), 2);
        damage_blob(&database.conn, &lost, how);

        // Then: a normal reader fails closed, naming the owner.
        let error = list_all_ordered(&database.conn).unwrap_err();
        let StorageError::BodyUnresolved { owner, .. } = &error else {
            panic!("expected BodyUnresolved, got {error:?}");
        };
        assert!(owner.contains(&lost_id.to_string()), "{owner}");
        assert!(list_by_session(&database.conn, "s1").is_err());

        // And: the deferred reader still hands both events over, with tokens in place.
        let blob_reads = Arc::new(AtomicU32::new(0));
        database
            .conn
            .authorizer(Some({
                let reads = Arc::clone(&blob_reads);
                move |context: rusqlite::hooks::AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Read {
                            table_name: "content_blobs",
                            column_name: "data"
                        }
                    ) {
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                    Authorization::Allow
                }
            }))
            .unwrap();
        let mut rows = Vec::new();
        for_each_event_ordered_deferred(&database.conn, i64::MAX, |row| rows.push(row)).unwrap();
        database
            .conn
            .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
            .unwrap();
        assert_eq!(
            blob_reads.load(Ordering::Relaxed),
            0,
            "the scan reads no blob data"
        );
        assert_eq!(rows.len(), 2);
        let DeferredRow::Event(first) = &rows[0] else {
            panic!("a deferred event");
        };
        assert_eq!(first.format, StoredFormat::Tokenized);
        let EventKind::Message(MessageEvent::MessageCompleted { text, .. }) = &first.event.kind
        else {
            panic!("not a completed message");
        };
        let reference = BodyRef::from_token(text).expect("a token in a tokenized event");
        assert_eq!(reference, BodyRef::from(BodyToken::of(&lost)));

        // And: resolving reports each body on its own.
        let healthy = BodyRef::from(BodyToken::of(&kept));
        let resolved = resolve_bodies(&database.conn, &[reference, healthy, reference]).unwrap();
        let failure = resolved[0].as_ref().unwrap_err();
        assert_eq!(
            failure.class,
            if how == "missing" {
                BodyFailureClass::Missing
            } else {
                BodyFailureClass::Corrupt
            }
        );
        assert!(!failure.is_transient());
        assert_eq!(resolved[1].as_ref().unwrap(), &kept);
        assert_eq!(
            resolved[2], resolved[0],
            "a repeated reference is read once"
        );
    }
}

#[test]
fn the_deferred_sequence_keeps_failures_in_place_and_resolves_in_bounded_batches() {
    // Given: a good event, a row that is not an event, and another good event.
    let database = database();
    let mut accounting = EventAccounting::default();
    append(&database, &mut accounting, completed("first"), 1);
    database
        .conn
        .execute(
            "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
             payload, subkind, run_id) VALUES ('s1', 1, 2, 2, 'Message', '{\"kind\":\"Message\",\"payload\":{\"kind\":\"Gone\"}}', 'Gone', 'run-a')",
            [],
        )
        .unwrap();
    append(&database, &mut accounting, completed("third"), 3);

    // When: read in sequence.
    let mut rows = Vec::new();
    for_each_event_ordered_deferred(&database.conn, i64::MAX, |row| rows.push(row)).unwrap();

    // Then: the failure sits between its neighbours with what its columns say.
    assert_eq!(rows.len(), 3);
    assert!(matches!(rows[0], DeferredRow::Event(_)));
    let DeferredRow::Failed(failure) = &rows[1] else {
        panic!("a failure in the middle");
    };
    assert_eq!(failure.run_id.as_deref(), Some("run-a"));
    assert_eq!(failure.subkind.as_deref(), Some("Gone"));
    assert!(matches!(
        failure.reason,
        StoredEventFailureReason::Variant(_)
    ));
    assert!(matches!(rows[2], DeferredRow::Event(_)));

    // And: one call resolves at most the stated number of distinct bodies.
    let many: Vec<BodyRef> = (0..=MAX_RESOLVE_REFS as u64)
        .map(|n| BodyRef::from(BodyToken::of(&format!("body {n}"))))
        .collect();
    assert!(resolve_bodies(&database.conn, &many).is_err());
    let at_the_limit = resolve_bodies(&database.conn, &many[..MAX_RESOLVE_REFS]).unwrap();
    assert!(at_the_limit.iter().all(Result::is_err));
}

#[test]
fn inspect_shows_bodies_resolved_or_as_references_and_filters_on_the_resolved_text() {
    let database = database();
    let mut accounting = EventAccounting::default();
    let needle = format!("{}FINDME{}", noise(40, 2_500), noise(41, 100));
    append(&database, &mut accounting, completed(&needle), 1);
    append(&database, &mut accounting, completed(&noise(42, 3_000)), 2);
    let window = EventScope::Window {
        from_ns: 0,
        to_ns: i64::MAX,
    };

    let blob_reads = Arc::new(AtomicU32::new(0));
    let (resolved, _) = database
        .inspect_events_with(
            window,
            10,
            InspectEventsOptions {
                bodies: InspectBodies::Resolved,
                contains: Some("FINDME"),
            },
        )
        .unwrap();
    assert_eq!(resolved.len(), 1, "the filter looks at the resolved body");
    assert_eq!(resolved[0].payload["payload"]["payload"]["text"], needle);

    database
        .conn
        .authorizer(Some({
            let reads = Arc::clone(&blob_reads);
            move |context: rusqlite::hooks::AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "content_blobs",
                        column_name: "data"
                    }
                ) {
                    reads.fetch_add(1, Ordering::Relaxed);
                }
                Authorization::Allow
            }
        }))
        .unwrap();
    let (refs, _) = database
        .inspect_events_with(
            window,
            10,
            InspectEventsOptions {
                bodies: InspectBodies::Refs,
                contains: None,
            },
        )
        .unwrap();
    database
        .conn
        .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(refs.len(), 2);
    let token = refs[0].payload["payload"]["payload"]["text"]
        .as_str()
        .unwrap();
    assert_eq!(
        BodyRef::from_token(token),
        Some(BodyRef::from(BodyToken::of(&needle)))
    );
    assert_eq!(
        blob_reads.load(Ordering::Relaxed),
        0,
        "refs mode reads no blob data"
    );

    damage_blob(&database.conn, &needle, "missing");
    let (broken, _) = database
        .inspect_events_with(window, 10, InspectEventsOptions::default())
        .unwrap();
    assert!(
        broken[0].body_error.is_some(),
        "an unresolvable body is reported, not fatal"
    );
    assert_eq!(broken.len(), 2);
}

#[test]
fn the_integrity_check_finds_missing_surplus_and_unreadable_event_bodies() {
    let database = database();
    let mut accounting = EventAccounting::default();
    let id = append(&database, &mut accounting, completed(&noise(50, 4_000)), 1);
    let other = append(&database, &mut accounting, completed(&noise(51, 4_000)), 2);
    let report =
        |conn: &Connection| check_bodies(conn, PRODUCTION_OWNERS, BodyScope::All, true).unwrap();
    assert!(report(&database.conn).is_ok());

    // A missing reference, a surplus one and an unreadable container.
    database
        .conn
        .execute(
            "DELETE FROM content_blob_refs_event WHERE event_id = ?1",
            [id],
        )
        .unwrap();
    let surplus = database
        .conn
        .execute(
            "INSERT INTO content_blob_refs_event(event_id, blob_id) \
             SELECT ?1, id FROM content_blobs WHERE id NOT IN (SELECT blob_id FROM content_blob_refs_event) LIMIT 1",
            [other],
        )
        .unwrap();
    assert_eq!(surplus, 1);
    database
        .conn
        .execute(
            "UPDATE events SET payload = 'not json' WHERE id = ?1",
            [other],
        )
        .unwrap();
    let kinds: Vec<String> = report(&database.conn)
        .problems
        .into_iter()
        .map(|p| p.kind.to_string())
        .collect();
    assert!(
        kinds.iter().any(|kind| kind == "container_json"),
        "{kinds:?}"
    );
}
