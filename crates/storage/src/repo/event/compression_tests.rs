//! Large event payloads are stored as zstd BLOBs; every reader must still see them.

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, CompactionEvent, DeliveryDisposition, Event,
    EventMeta, MessageEvent, ToolEvent,
};
use rusqlite::Connection;

use super::{EventAccounting, append_event, encode_payload, list_all_ordered, list_by_session};
use crate::inspect::EventScope;
use crate::{Database, HardLimits, LimitKind, StorageError};

fn event_at(kind: impl Into<event_bus::EventKind>, nanos: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_nanos(nanos),
            wall_clock: UNIX_EPOCH + Duration::from_nanos(nanos),
        },
        kind: kind.into(),
    }
}

fn big(seed: &str) -> String {
    format!("{seed} compressible output line\n").repeat(400)
}

fn tool_completed(
    output: String,
    run_id: Option<&str>,
    detail: Option<serde_json::Value>,
) -> Event {
    event_at(
        ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
            is_error: false,
            output: Some(output),
            detail,
            run_id: run_id.map(String::from),
            parent_call_id: None,
        },
        1,
    )
}

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    crate::migrations::apply_migrations(&connection).unwrap();
    connection
        .execute(
            "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) \
             VALUES ('s1', 'running', 0, 0)",
            [],
        )
        .unwrap();
    connection
}

fn append(connection: &Connection, accounting: &mut EventAccounting, event: &Event) -> i64 {
    append_event(
        connection,
        Some("s1"),
        event,
        &HardLimits::default(),
        accounting,
    )
    .unwrap()
    .id
}

fn storage_class(connection: &Connection, id: i64) -> String {
    connection
        .query_row(
            "SELECT typeof(payload) FROM events WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn bulky_kinds_are_compressed_and_everything_else_stays_text() {
    // Given: large and small events of the compressible kinds, and a large agent message.
    let connection = fixture();
    let mut accounting = EventAccounting::default();
    let agent_message = event_at(
        AgentMessageEvent::Delivered {
            message: AgentMessage {
                message_id: "msg-1".into(),
                sender_run_id: "run-a".into(),
                recipient_run_id: "run-b".into(),
                kind: AgentMessageKind::Send,
                content: big("agent"),
                reply_to: None,
            },
            disposition: DeliveryDisposition::Steering,
        },
        2,
    );
    let compaction = event_at(
        CompactionEvent::Compacted {
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
            summary: big("summary"),
        },
        3,
    );
    let large = [
        tool_completed(big("tool"), Some("run-a"), None),
        event_at(
            MessageEvent::MessageDelta {
                delta: big("message"),
                run_id: Some("run-a".into()),
            },
            4,
        ),
        compaction,
    ];
    let small = tool_completed("short".into(), Some("run-a"), None);

    // When: they are appended and read back.
    let agent_message_id = append(&connection, &mut accounting, &agent_message);
    let large_ids: Vec<_> = large
        .iter()
        .map(|event| append(&connection, &mut accounting, event))
        .collect();
    let small_id = append(&connection, &mut accounting, &small);
    let stored = list_by_session(&connection, "s1").unwrap();

    // Then: large ones are BLOBs, the small one is TEXT, and all decode to the original.
    for id in &large_ids {
        assert_eq!(storage_class(&connection, *id), "blob");
    }
    assert_eq!(storage_class(&connection, small_id), "text");
    assert_eq!(storage_class(&connection, agent_message_id), "text");
    let expected: Vec<_> = [&agent_message]
        .into_iter()
        .chain(large.iter())
        .chain([&small])
        .collect();
    assert_eq!(
        stored.iter().map(|row| &row.event).collect::<Vec<_>>(),
        expected
    );
    let text = big("orchestrator");
    for (kind, subkind) in [
        ("Orchestrator", "TaskProgressed"),
        ("Lifecycle", "Started"),
        ("Diagnostic", ""),
        ("Provider", "RequestCompleted"),
        ("Fault", "Raised"),
        ("AgentMessage", "Delivered"),
        ("Tool", "ApprovalRequested"),
        ("Tool", "ApprovalResolved"),
        ("Tool", "ExecutionDenied"),
    ] {
        assert!(
            matches!(
                encode_payload(kind, subkind, &text),
                crate::codec::Encoded::Plain(_)
            ),
            "{kind}/{subkind} must stay TEXT"
        );
    }
    assert!(matches!(
        encode_payload("Tool", "ToolCompleted", &text),
        crate::codec::Encoded::Compressed(_)
    ));
}

#[test]
fn accounting_charges_stored_bytes_while_the_event_limit_checks_plaintext() {
    // Given: a compressible event far larger compressed than its plaintext.
    let connection = fixture();
    let mut accounting = EventAccounting::default();
    let event = tool_completed(big("accounting"), Some("run-a"), None);
    let plaintext = u64::try_from(serde_json::to_string(&event.kind).unwrap().len()).unwrap();

    // When: it is appended.
    let id = append(&connection, &mut accounting, &event);

    // Then: every counter agrees with the bytes in the database, not the plaintext.
    let stored: i64 = connection
        .query_row(
            "SELECT OCTET_LENGTH(payload) FROM events WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    let stored = u64::try_from(stored).unwrap();
    assert!(stored < plaintext / 4, "{stored} vs {plaintext}");
    let aggregate: i64 = connection
        .query_row(
            "SELECT payload_bytes FROM event_session_bytes WHERE session_id = 's1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let session_total: i64 = connection
        .query_row(
            "SELECT total_event_bytes FROM sessions WHERE id = 's1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(u64::try_from(aggregate).unwrap(), stored);
    assert_eq!(u64::try_from(session_total).unwrap(), stored);
    assert_eq!(accounting.session_bytes, stored);
    assert_eq!(accounting.day_bytes, stored);

    // And: the per-event limit still refuses by plaintext size.
    let limits = HardLimits {
        max_event_bytes: plaintext - 1,
        ..HardLimits::default()
    };
    let error = append_event(
        &connection,
        Some("s1"),
        &tool_completed(big("accounting"), Some("run-a"), None),
        &limits,
        &mut EventAccounting::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StorageError::LimitExceeded {
            limit: LimitKind::EventSize,
            ..
        }
    ));
}

#[test]
fn plain_and_compressed_rows_mix_in_one_history() {
    // Given: a legacy plaintext row of a large event written before compression existed.
    let connection = fixture();
    let mut accounting = EventAccounting::default();
    let legacy = tool_completed(big("legacy"), Some("run-a"), None);
    connection
        .execute(
            "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
             payload, subkind, run_id) VALUES ('s1', ?1, 1, 1, 'Tool', ?2, 'ToolCompleted', 'run-a')",
            rusqlite::params![
                legacy.meta.schema_version,
                serde_json::to_string(&legacy.kind).unwrap()
            ],
        )
        .unwrap();

    // When: a compressed row follows it.
    let fresh = tool_completed(big("fresh"), Some("run-a"), None);
    let fresh_id = append(&connection, &mut accounting, &fresh);

    // Then: both are readable in order, through every list entry point.
    assert_eq!(storage_class(&connection, fresh_id), "blob");
    let all = list_all_ordered(&connection).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].event.kind, legacy.kind);
    assert_eq!(all[1].event.kind, fresh.kind);
    let mut visited = 0;
    super::for_each_ordered(&connection, |_| visited += 1).unwrap();
    assert_eq!(visited, 2);
    let (by_run, truncated) =
        super::select::list_for_runs(&connection, &["run-a".to_owned()], i64::MIN, 10).unwrap();
    assert_eq!((by_run.len(), truncated), (2, false));
}

#[test]
fn events_that_only_the_payload_can_attribute_are_stored_as_text_and_found() {
    // Given: a large agent message between two runs; its columns name neither.
    let connection = fixture();
    let mut accounting = EventAccounting::default();
    let message = event_at(
        AgentMessageEvent::Delivered {
            message: AgentMessage {
                message_id: "msg-1".into(),
                sender_run_id: "run-sender".into(),
                recipient_run_id: "run-recipient".into(),
                kind: AgentMessageKind::Send,
                content: big("agent"),
                reply_to: None,
            },
            disposition: DeliveryDisposition::Steering,
        },
        1,
    );
    let id = append(&connection, &mut accounting, &message);
    assert_eq!(storage_class(&connection, id), "text");

    // When/Then: both ends find it, and an unrelated run does not.
    for run in ["run-sender", "run-recipient"] {
        let (events, _) =
            super::select::list_for_runs(&connection, &[run.to_owned()], i64::MIN, 10).unwrap();
        assert_eq!(events.len(), 1, "{run}");
    }
    let (events, _) =
        super::select::list_for_runs(&connection, &["run-other".to_owned()], i64::MIN, 10).unwrap();
    assert!(events.is_empty());
}

#[test]
fn inspecting_a_run_confirms_compressed_blank_and_unconverted_rows_by_decoding() {
    // Given: compressed Tool rows that name run-target (or another run) only inside the payload,
    // one marked blank (`subkind = ''`) and one not yet backfilled (`subkind IS NULL`).
    let database = Database::open_in_memory().unwrap();
    let mut accounting = EventAccounting::default();
    database
        .conn
        .execute(
            "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) \
             VALUES ('s1', 'running', 0, 0)",
            [],
        )
        .unwrap();
    let mut insert = |detail_run: &str, subkind: Option<&str>| {
        let id = append(
            &database.conn,
            &mut accounting,
            &tool_completed(
                big("inspect"),
                None,
                Some(serde_json::json!({ "run_id": detail_run })),
            ),
        );
        assert_eq!(storage_class(&database.conn, id), "blob");
        database
            .conn
            .execute(
                "UPDATE events SET subkind = ?2, run_id = NULL WHERE id = ?1",
                rusqlite::params![id, subkind],
            )
            .unwrap();
        id
    };
    let blank = insert("run-target", Some(""));
    let unconverted = insert("run-target", None);
    let blank_other = insert("run-other", Some(""));
    let unconverted_other = insert("run-other", None);

    // When: the run's events are inspected.
    let (events, truncated) = database
        .inspect_events(EventScope::Run("run-target"), 100)
        .unwrap();

    // Then: only the rows whose decoded payload names the run are returned, decoded.
    let ids: Vec<_> = events.iter().map(|event| event.id).collect();
    assert!(!truncated);
    assert_eq!(ids, vec![blank, unconverted]);
    assert!(!ids.contains(&blank_other) && !ids.contains(&unconverted_other));
    assert_eq!(
        events[0].payload["payload"]["payload"]["detail"]["run_id"],
        "run-target"
    );
}

fn decoded_blobs_during<T>(action: impl FnOnce() -> T) -> (T, usize) {
    let before = crate::codec::tests::DECODED_BLOBS.with(std::cell::Cell::get);
    let result = action();
    let after = crate::codec::tests::DECODED_BLOBS.with(std::cell::Cell::get);
    (result, after - before)
}

#[test]
fn a_run_lookup_decodes_only_the_compressed_rows_of_that_run() {
    // Given: one compressed event of the target run among compressed events of other runs,
    // large agent messages, and large approvals-free tool traffic.
    let connection = fixture();
    let mut accounting = EventAccounting::default();
    let target = tool_completed(big("target"), Some("run-target"), None);
    append(&connection, &mut accounting, &target);
    let unrelated = |count: usize, connection: &Connection, accounting: &mut EventAccounting| {
        for index in 0..count {
            append(
                connection,
                accounting,
                &tool_completed(big(&format!("other-{index}")), Some("run-other"), None),
            );
        }
    };
    let lookup = |connection: &Connection| {
        decoded_blobs_during(|| {
            super::select::list_for_runs(connection, &["run-target".to_owned()], i64::MIN, 100)
                .unwrap()
                .0
        })
    };
    unrelated(3, &connection, &mut accounting);
    let (few, few_decodes) = lookup(&connection);

    // When: many more unrelated compressed rows are added and the run is looked up again.
    unrelated(60, &connection, &mut accounting);
    let (many, many_decodes) = lookup(&connection);

    // Then: the same single event is returned and the number of decoded rows did not grow.
    assert_eq!((few.len(), many.len()), (1, 1));
    assert_eq!(many[0].event, target);
    assert_eq!((few_decodes, many_decodes), (1, 1));
}
