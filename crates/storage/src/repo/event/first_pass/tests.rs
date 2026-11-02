//! The first pass must answer exactly what the full replay's own first pass
//! answers, while decoding only what it has to.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use event_bus::{
    AgentRunPhase, DiagnosticEvent, DiagnosticSeverity, Event, EventKind, LifecycleEvent,
    MessageEvent, MessagePartAssembler, OrchestratorEvent, ProviderEvent, ThreadTodoSnapshot,
    ToolEvent,
};
use rusqlite::Connection;

use super::*;
use crate::repo::event::{EventAccounting, append_event};
use crate::{Database, HardLimits};

#[derive(Debug, PartialEq)]
struct Answer {
    last_waiting: BTreeMap<String, i64>,
    completed_at_waiting: BTreeMap<String, bool>,
    replay_end: Option<SystemTime>,
    task_events: Vec<i64>,
}

/// The first pass as `restore_history` ran it before this scan existed: decode
/// every event, reassemble stored responses, then look at what is left.
fn full_replay(conn: &Connection) -> Answer {
    let mut answer = Answer {
        last_waiting: BTreeMap::new(),
        completed_at_waiting: BTreeMap::new(),
        replay_end: None,
        task_events: Vec::new(),
    };
    let mut completed_since_request = BTreeMap::new();
    let mut parts = MessagePartAssembler::new();
    crate::repo::event::for_each_ordered(conn, |stored| {
        let Some(assembled) = parts.accept(&stored.event) else {
            return;
        };
        let event: &Event = &assembled;
        if is_task_event(event) {
            answer.task_events.push(stored.id);
        }
        answer.replay_end = Some(event.meta.wall_clock);
        match &event.kind {
            EventKind::Provider(ProviderEvent::RequestStarted {
                run_id: Some(run_id),
                ..
            }) => {
                completed_since_request.insert(run_id.clone(), false);
            }
            EventKind::Message(MessageEvent::MessageCompleted { run_id, .. }) => {
                completed_since_request.insert(run_id.clone(), true);
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Waiting,
                ..
            }) => {
                answer.last_waiting.insert(run_id.clone(), stored.id);
                answer.completed_at_waiting.insert(
                    run_id.clone(),
                    completed_since_request
                        .get(run_id)
                        .copied()
                        .unwrap_or(false),
                );
            }
            _ => {}
        }
    })
    .unwrap();
    answer
}

type Failures = Vec<(i64, Option<String>)>;

fn first_pass(conn: &Connection) -> (Answer, FirstPassMetrics, Failures) {
    let mut task_events = Vec::new();
    let mut failures = Vec::new();
    let scan = scan(
        conn,
        |stored| task_events.push(stored.id),
        |id, run, _| failures.push((id, run.map(str::to_owned))),
    )
    .unwrap();
    (
        Answer {
            last_waiting: scan.last_waiting,
            completed_at_waiting: scan.completed_at_waiting,
            replay_end: scan.replay_end,
            task_events,
        },
        scan.metrics,
        failures,
    )
}

fn at(seconds: u64, mut event: Event) -> Event {
    event.meta.wall_clock = UNIX_EPOCH + Duration::from_secs(seconds);
    event
}

fn append(conn: &Connection, event: &Event) -> i64 {
    append_event(
        conn,
        Some("session"),
        event,
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap()
    .id
}

fn wall_clock_of(conn: &Connection, id: i64) -> SystemTime {
    let ns: i64 = conn
        .query_row(
            "SELECT wall_clock_ns FROM events WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    crate::db::ns_to_system_time(ns)
}

fn phase(run: &str, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: AgentRunPhase::Running,
        to,
        reason: None,
    })
}

fn started(run: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: Some("root".into()),
        agent_name: "worker".into(),
        role: "Worker".into(),
    })
}

fn request(run: &str) -> Event {
    Event::new(ProviderEvent::RequestStarted {
        request_id: format!("request-{run}"),
        provider: "p".into(),
        profile: None,
        protocol: "p".into(),
        model: "m".into(),
        streaming: true,
        run_id: Some(run.into()),
        reasoning_effort: None,
        purpose: None,
    })
}

fn completed(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: text.into(),
    })
}

fn part(run: &str, response: &str, index: usize, total: usize, text: &str) -> Event {
    Event::new(MessageEvent::MessagePart {
        run_id: run.into(),
        response_id: response.into(),
        text: text.into(),
        index,
        total,
    })
}

/// Large enough that the codec stores it compressed.
fn bulky(label: &str) -> String {
    format!("{label} bulky body ").repeat(2048)
}

fn blobs_decoded() -> usize {
    crate::codec::tests::DECODED_BLOBS.with(std::cell::Cell::get)
}

/// A history that exercises every rule the first pass mirrors: repeated
/// waits, a wait before and after a completion, stored responses (complete,
/// interleaved across runs, interrupted by a run's own event and by a session
/// event, out of order), and bulky rows that must never be opened.
fn mixed_history(conn: &Connection) {
    let events = [
        started("child-1"),
        request("child-1"),
        completed("child-1", &bulky("first answer")),
        phase("child-1", AgentRunPhase::Waiting),
        Event::new(LifecycleEvent::TurnCompleted {
            run_id: "child-1".into(),
            context_len: 2,
        }),
        request("child-1"),
        Event::new(ToolEvent::ToolCompleted {
            run_id: Some("child-1".into()),
            tool_name: "shell".into(),
            call_id: "call".into(),
            output: Some(bulky("tool output")),
            detail: None,
            is_error: false,
            parent_call_id: None,
        }),
        phase("child-1", AgentRunPhase::Waiting),
        started("child-2"),
        request("child-2"),
        part("child-2", "r1", 0, 3, &bulky("alpha")),
        part("child-3", "r9", 0, 2, "other run"),
        part("child-2", "r1", 1, 3, &bulky("beta")),
        part("child-3", "r9", 1, 2, "finishes first"),
        part("child-2", "r1", 2, 3, "tail"),
        phase("child-2", AgentRunPhase::Waiting),
        // Interrupted by the run's own event, then restarted and finished.
        part("child-4", "r2", 0, 2, "lost"),
        request("child-4"),
        part("child-4", "r2", 1, 2, "orphan"),
        part("child-4", "r3", 0, 2, "again"),
        part("child-4", "r3", 1, 2, "done"),
        // Closed by a session event, not by a run event.
        part("child-5", "r4", 0, 2, "closed"),
        Event::new(LifecycleEvent::Failed {
            session_id: "child-5".into(),
            reason: "boom".into(),
        }),
        part("child-5", "r4", 1, 2, "late"),
        // Out of order, and another response in the middle of a sequence.
        part("child-6", "r5", 0, 3, "a"),
        part("child-6", "r5", 2, 3, "c"),
        part("child-6", "r6", 1, 3, "x"),
        phase("child-6", AgentRunPhase::Waiting),
        phase("child-6", AgentRunPhase::Done),
        Event::new(ToolEvent::ApprovalRequested {
            tool_name: "write".into(),
            call_id: "pending".into(),
            input: None,
        }),
    ];
    for (n, event) in events.into_iter().enumerate() {
        // One wall clock runs backwards: the replay end is the last event in
        // ID order, not the greatest time.
        let seconds = if n == 11 { 1 } else { 100 + n as u64 };
        append(conn, &at(seconds, event));
    }
}

#[test]
fn scan_answers_what_the_full_replay_answers_and_opens_only_what_it_needs() {
    let db = Database::open_in_memory().unwrap();
    mixed_history(&db.conn);
    let expected = full_replay(&db.conn);
    assert_eq!(expected.last_waiting.len(), 3);

    let opened_before = blobs_decoded();
    let (answer, metrics, failures) = first_pass(&db.conn);
    let opened = blobs_decoded() - opened_before;

    assert_eq!(answer, expected);
    assert!(failures.is_empty());
    assert!(!metrics.used_fallback);
    assert_eq!(metrics.waiting_event_decodes, 0);
    assert_eq!(metrics.unclassified_event_decodes, 0);
    assert_eq!(metrics.fallback_events_decoded, 0);
    assert_eq!(metrics.session_marker_rows, 1);
    assert_eq!(
        metrics.task_event_decodes,
        expected.task_events.len() as u64
    );
    assert_eq!(metrics.message_part_metadata_rows, 14);
    // The only compressed rows opened are the two bulky parts; the bulky
    // MessageCompleted and ToolCompleted rows stay closed.
    assert_eq!(opened, 2);
    assert!(metrics.message_part_expanded_bytes > metrics.message_part_stored_bytes);
}

#[test]
fn what_is_decoded_does_not_grow_with_bodies_the_pass_has_no_use_for() {
    let small = Database::open_in_memory().unwrap();
    let large = Database::open_in_memory().unwrap();
    for (db, repeat) in [(&small, 1), (&large, 6)] {
        for n in 0..20 {
            let run = format!("run-{n}");
            append(&db.conn, &started(&run));
            append(&db.conn, &request(&run));
            append(&db.conn, &completed(&run, &bulky("answer").repeat(repeat)));
            append(&db.conn, &phase(&run, AgentRunPhase::Waiting));
        }
    }
    let opened_before = blobs_decoded();
    let (_, small_metrics, _) = first_pass(&small.conn);
    let (_, large_metrics, _) = first_pass(&large.conn);
    assert_eq!(blobs_decoded(), opened_before);
    assert_eq!(small_metrics, large_metrics);
    assert_eq!(small_metrics.task_event_decodes, 40);
}

#[test]
fn an_unfilled_subkind_falls_back_to_decoding_every_row_with_the_same_answer() {
    let db = Database::open_in_memory().unwrap();
    mixed_history(&db.conn);
    let expected = full_replay(&db.conn);
    let rows: u64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    db.conn
        .execute(
            "UPDATE events SET subkind = NULL, run_id = NULL WHERE id % 2 = 0",
            [],
        )
        .unwrap();

    let (answer, metrics, failures) = first_pass(&db.conn);

    assert_eq!(answer, expected);
    assert!(failures.is_empty());
    assert!(metrics.used_fallback);
    assert_eq!(metrics.fallback_events_decoded, rows);
    assert!(metrics.fallback_payload_bytes > 0);
    assert_eq!(metrics.task_event_decodes, 0);
    assert_eq!(metrics.message_part_metadata_rows, 0);
}

#[test]
fn one_unreadable_row_is_skipped_and_reported_without_losing_the_rest() {
    let db = Database::open_in_memory().unwrap();
    let ids: Vec<i64> = [
        started("child-1"),
        request("child-1"),
        phase("child-1", AgentRunPhase::Waiting),
        part("child-2", "r1", 0, 2, "a"),
        part("child-2", "r1", 1, 2, "b"),
        completed("child-1", "answer"),
        phase("child-1", AgentRunPhase::Waiting),
    ]
    .iter()
    .map(|event| append(&db.conn, event))
    .collect();
    // A task row whose JSON is broken and a part whose BLOB is not a zstd frame.
    db.conn
        .execute(
            "UPDATE events SET payload = '{broken' WHERE id = ?1",
            [ids[2]],
        )
        .unwrap();
    db.conn
        .execute(
            "UPDATE events SET payload = x'0102030405' WHERE id = ?1",
            [ids[3]],
        )
        .unwrap();

    let (answer, metrics, failures) = first_pass(&db.conn);

    assert_eq!(
        failures,
        vec![
            (ids[2], Some("child-1".to_owned())),
            (ids[3], Some("child-2".to_owned())),
        ]
    );
    assert_eq!(metrics.isolated_events, 2);
    // The broken wait is gone and the later one stands; the response whose
    // first part is unreadable never completes.
    assert_eq!(answer.last_waiting["child-1"], ids[6]);
    assert!(answer.completed_at_waiting["child-1"]);
    assert_eq!(answer.task_events, vec![ids[0], ids[6]]);
    assert_eq!(answer.replay_end, Some(wall_clock_of(&db.conn, ids[6])));
}

#[test]
fn rows_of_a_deleted_run_neither_move_the_replay_end_nor_are_reported() {
    let db = Database::open_in_memory().unwrap();
    let keep = append(&db.conn, &at(10, phase("kept", AgentRunPhase::Waiting)));
    append(&db.conn, &at(30, part("gone", "r", 0, 1, "text")));
    append(&db.conn, &at(40, completed("gone", "tomb")));
    let broken = append(&db.conn, &at(50, phase("gone", AgentRunPhase::Waiting)));
    db.conn
        .execute("UPDATE events SET payload = '{' WHERE id = ?1", [broken])
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('gone', 't', 'f')",
            [],
        )
        .unwrap();

    let expected = full_replay(&db.conn);
    let (answer, _, failures) = first_pass(&db.conn);

    assert_eq!(answer, expected);
    assert_eq!(
        answer.replay_end,
        Some(UNIX_EPOCH + Duration::from_secs(10))
    );
    assert_eq!(answer.last_waiting.keys().collect::<Vec<_>>(), ["kept"]);
    assert_eq!(answer.last_waiting["kept"], keep);
    assert!(failures.is_empty());
}

#[test]
fn a_deleted_threads_todo_snapshot_does_not_move_the_replay_end() {
    let db = Database::open_in_memory().unwrap();
    append(&db.conn, &at(10, phase("kept", AgentRunPhase::Waiting)));
    append(
        &db.conn,
        &at(
            90,
            Event::new(OrchestratorEvent::ThreadTodoUpdated {
                snapshot: ThreadTodoSnapshot {
                    list_id: "list".into(),
                    thread_id: "gone-thread".into(),
                    revision: 1,
                    items: vec![],
                },
            }),
        ),
    );
    let (before, _, _) = first_pass(&db.conn);
    assert_eq!(
        before.replay_end,
        Some(UNIX_EPOCH + Duration::from_secs(90))
    );

    db.conn
        .execute(
            "INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) \
             VALUES ('gone-thread', 'f', 0)",
            [],
        )
        .unwrap();
    let expected = full_replay(&db.conn);
    let (answer, metrics, _) = first_pass(&db.conn);

    assert_eq!(answer, expected);
    assert_eq!(
        answer.replay_end,
        Some(UNIX_EPOCH + Duration::from_secs(10))
    );
    assert_eq!(metrics.thread_probe_decodes, 1);
}

#[test]
fn events_without_an_inner_tag_are_not_mistaken_for_unreadable_ones() {
    let db = Database::open_in_memory().unwrap();
    append(&db.conn, &at(10, phase("run", AgentRunPhase::Waiting)));
    let id = append(
        &db.conn,
        &at(
            20,
            Event::new(DiagnosticEvent {
                source: "test".into(),
                severity: DiagnosticSeverity::Warning,
                code: "code".into(),
                detail: "detail".into(),
                run_id: None,
                thread_id: None,
                call_id: None,
            }),
        ),
    );
    let subkind: String = db
        .conn
        .query_row("SELECT subkind FROM events WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(subkind, "");

    let (answer, metrics, failures) = first_pass(&db.conn);

    assert_eq!(answer, full_replay(&db.conn));
    assert_eq!(
        answer.replay_end,
        Some(UNIX_EPOCH + Duration::from_secs(20))
    );
    assert!(failures.is_empty());
    assert_eq!(metrics.unclassified_event_decodes, 0);
}

#[test]
fn an_event_that_no_variant_fits_is_decoded_once_and_isolated() {
    let db = Database::open_in_memory().unwrap();
    let wait = append(&db.conn, &at(10, phase("run", AgentRunPhase::Waiting)));
    let unknown = append(&db.conn, &at(20, completed("run", "text")));
    // What the backfill leaves behind for a payload that no longer decodes.
    db.conn
        .execute(
            "UPDATE events SET subkind = '', run_id = NULL, \
             payload = '{\"kind\":\"Message\",\"payload\":{\"kind\":\"Gone\"}}' WHERE id = ?1",
            [unknown],
        )
        .unwrap();

    let (answer, metrics, failures) = first_pass(&db.conn);

    assert_eq!(answer, full_replay(&db.conn));
    assert_eq!(answer.replay_end, Some(wall_clock_of(&db.conn, wait)));
    assert_eq!(failures, vec![(unknown, None)]);
    assert_eq!(metrics.unclassified_event_decodes, 1);
}

#[test]
fn isolated_replay_reports_a_bad_row_and_hands_over_every_other() {
    let db = Database::open_in_memory().unwrap();
    let first = append(&db.conn, &started("child-1"));
    let bad = append(&db.conn, &completed("child-1", "text"));
    let last = append(&db.conn, &phase("child-1", AgentRunPhase::Waiting));
    db.conn
        .execute("UPDATE events SET payload = 'nope' WHERE id = ?1", [bad])
        .unwrap();
    let mut seen = Vec::new();
    let mut failed = Vec::new();
    for_each_isolated(
        &db.conn,
        |stored| seen.push(stored.id),
        |id, run, reason| failed.push((id, run.map(str::to_owned), reason.is_empty())),
    )
    .unwrap();
    assert_eq!(seen, vec![first, last]);
    assert_eq!(failed, vec![(bad, Some("child-1".to_owned()), false)]);
}

#[test]
fn reading_writes_nothing() {
    let db = Database::open_in_memory().unwrap();
    mixed_history(&db.conn);
    let changes = |db: &Database| -> i64 {
        db.conn
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .unwrap()
    };
    let before = changes(&db);
    first_pass(&db.conn);
    assert_eq!(changes(&db), before);
}

/// The tracker and the assembler it mirrors, fed the same stream, must agree
/// on every step. A fixed pseudo-random stream covers interleaving runs,
/// restarts, gaps, switches of response and interruptions, and more runs than
/// the tracker may keep.
#[test]
fn part_tracker_agrees_with_the_assembler_on_arbitrary_streams() {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    for _ in 0..200 {
        let mut assembler = MessagePartAssembler::new();
        let mut tracker = PartTracker::new();
        for step in 0..160 {
            let run = format!("run-{}", next(70));
            let event = if next(5) == 0 {
                request(&run)
            } else {
                let response = format!("resp-{}", next(2));
                let total = next(4) as usize;
                let index = next(4) as usize;
                let text = "t".repeat(next(4) as usize);
                part(&run, &response, index, total, &text)
            };
            let completes = assembler.accept(&event).is_some_and(|accepted| {
                matches!(
                    event.kind,
                    EventKind::Message(MessageEvent::MessagePart { .. })
                ) && matches!(
                    accepted.kind,
                    EventKind::Message(MessageEvent::MessageCompleted { .. })
                )
            });
            let tracked = match &event.kind {
                EventKind::Message(MessageEvent::MessagePart {
                    run_id,
                    response_id,
                    text,
                    index,
                    total,
                }) => tracker.accept(run_id, response_id, *index, *total, text.len()),
                other => {
                    tracker.interrupt(other.primary_run_id().unwrap());
                    false
                }
            };
            assert_eq!(tracked, completes, "step {step}: {event:?}");
        }
    }
}

#[test]
fn part_tracker_keeps_at_most_the_run_limit_and_evicts_the_oldest() {
    let mut tracker = PartTracker::with_limits(2, 100);
    assert!(!tracker.accept("a", "r", 0, 2, 1));
    assert!(!tracker.accept("b", "r", 0, 2, 1));
    // A third run evicts "a", the oldest.
    assert!(!tracker.accept("c", "r", 0, 2, 1));
    assert_eq!(tracker.open.len(), 2);
    assert!(!tracker.accept("a", "r", 1, 2, 1));
    assert!(tracker.accept("c", "r", 1, 2, 1));
    assert_eq!(tracker.buffered_bytes, 1);
}

#[test]
fn part_tracker_counts_bytes_not_text_and_drops_a_response_over_the_budget() {
    let mut tracker = PartTracker::with_limits(4, 10);
    assert!(!tracker.accept("a", "r", 0, 2, 6));
    assert!(!tracker.accept("b", "r", 0, 2, 4));
    assert_eq!(tracker.buffered_bytes, 10);
    // One byte over the shared budget drops the response that asked for it.
    assert!(!tracker.accept("a", "r", 1, 2, 1));
    assert_eq!(tracker.buffered_bytes, 4);
    assert!(tracker.accept("b", "r", 1, 2, 6));
    assert_eq!(tracker.buffered_bytes, 0);
}

#[test]
fn the_default_byte_budget_matches_the_assembler_at_its_edge() {
    let mut assembler = MessagePartAssembler::new();
    let mut tracker = PartTracker::new();
    for len in [MAX_PENDING_BYTES, MAX_PENDING_BYTES + 1] {
        let event = part("run", "r", 0, 1, &"x".repeat(len));
        assert_eq!(
            tracker.accept("run", "r", 0, 1, len),
            assembler.accept(&event).is_some(),
            "{len} bytes"
        );
    }
}

#[test]
fn only_the_documented_rows_are_decoded() {
    for (kind, subkind, probe, expected) in [
        ("Lifecycle", "AgentRunStateChanged", false, "task"),
        ("Lifecycle", "TurnCompleted", false, "task"),
        ("Lifecycle", "Started", false, "session"),
        ("Message", "MessagePart", false, "part"),
        ("Message", "MessageCompleted", false, "columns"),
        ("Tool", "ToolCompleted", false, "columns"),
        ("Provider", "RequestStarted", false, "columns"),
        ("Diagnostic", "", false, "columns"),
        ("Tool", "", false, "unclassified"),
        ("Orchestrator", "ThreadTodoUpdated", false, "columns"),
        ("Orchestrator", "ThreadTodoUpdated", true, "probe"),
    ] {
        let label = match classify(kind, Some(subkind), probe) {
            Reading::Columns => "columns",
            Reading::Part => "part",
            Reading::SessionMarker => "session",
            Reading::Decode(Decoding::Task) => "task",
            Reading::Decode(Decoding::Unclassified) => "unclassified",
            Reading::Decode(Decoding::ThreadProbe) => "probe",
        };
        assert_eq!(label, expected, "{kind}/{subkind}");
    }
    assert!(matches!(
        classify("Message", None, false),
        Reading::Decode(Decoding::Unclassified)
    ));
}
