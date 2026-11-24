use super::Role;
use super::read::*;
use crate::{
    Database, HardLimits,
    repo::event::{EventAccounting, append_event},
};
use event_bus::{Event, EventKind, LifecycleEvent, MessageEvent};
use rusqlite::params;
use std::time::{Duration, UNIX_EPOCH};

fn scope(db: &Database) {
    db.conn
        .execute_batch(
            "INSERT INTO threads(thread_id,root_run_id,created_at_ns,updated_at_ns,source)
         VALUES ('thread','run',0,0,'runtime');
         INSERT INTO thread_runs VALUES ('run','thread','run',0);",
        )
        .unwrap();
}
fn event(db: &Database, at: u64, kind: impl Into<EventKind>) -> i64 {
    let mut event = Event::new(kind);
    event.meta.wall_clock = UNIX_EPOCH + Duration::from_nanos(at);
    append_event(
        &db.conn,
        None,
        &event,
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap()
    .id
}
fn answer(db: &Database, at: u64, text: &str) -> i64 {
    event(
        db,
        at,
        MessageEvent::MessageCompleted {
            run_id: "run".into(),
            text: text.into(),
        },
    )
}
fn receipt(db: &Database, id: &str, seq: i64, at: i64, status: &str, text: &str) {
    db.conn.execute(
        "INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns,acceptance_seq)
         VALUES (?1,'run','run',?2,?3,?4,0,?5)",
        params![id,text,status,at,seq],
    ).unwrap();
}
fn prompt() -> LifecycleEvent {
    LifecycleEvent::TaskPromptPublished {
        run_id: "run".into(),
        parent_run_id: None,
        agent_name: "worker".into(),
        role: "Worker".into(),
        prompt: "prompt".into(),
        user_text: None,
        skills: vec![],
    }
}
fn prepared<'a>(
    snapshot: &'a HistorySnapshot<'_>,
    h: i64,
    direction: Direction,
) -> PreparedHistory<'a> {
    match snapshot.prepare(h, direction).unwrap() {
        Availability::Indexed(p) => p,
        Availability::Legacy(reason) => panic!("not indexed: {reason}"),
    }
}
fn words(page: HistoryPage) -> Vec<(u64, Role, String)> {
    page.items
        .into_iter()
        .map(|d| (d.index, d.role, d.parts.into_iter().map(|p| p.0).collect()))
        .collect()
}

#[test]
fn initial_input_uses_all_status_identity_but_clock_order_for_indices() {
    for status in ["accepted", "withdrawn", "unknown", "delivered"] {
        for at in [9, 10, 11] {
            let db = Database::open_in_memory().unwrap();
            scope(&db);
            receipt(&db, "first", 1, at, status, "initial");
            receipt(&db, "later", 2, 8, "delivered", "followup");
            let f = event(&db, 10, prompt());
            answer(&db, 13, "answer");
            let snapshot = db.history_snapshot("thread", None).unwrap().unwrap();
            let p = prepared(&snapshot, snapshot.latest, Direction::Forward);
            let page = p.page(Direction::Forward, None, 50, 768 * 1024).unwrap();
            let mut expected = vec![(0, Role::User, "followup".into())];
            let suppress = status == "delivered" && at <= 10;
            if status == "delivered" && at <= 10 {
                expected.push((1, Role::User, "initial".into()));
            }
            if !suppress {
                expected.push((expected.len() as u64, Role::User, "prompt".into()));
            }
            if status == "delivered" && at > 10 {
                expected.push((expected.len() as u64, Role::User, "initial".into()));
            }
            expected.push((expected.len() as u64, Role::Assistant, "answer".into()));
            assert_eq!(words(page), expected, "{status}/{at}");
            let before = prepared(&snapshot, f - 1, Direction::Forward);
            let expected = if status == "delivered" {
                vec![
                    (0, Role::User, "followup".into()),
                    (1, Role::User, "initial".into()),
                ]
            } else {
                vec![(0, Role::User, "followup".into())]
            };
            assert_eq!(
                words(
                    before
                        .page(Direction::Forward, None, 50, 768 * 1024)
                        .unwrap()
                ),
                expected
            );
        }
    }
}

#[test]
fn nonmonotonic_clocks_use_nonoutput_cuts_and_historical_tail_counts() {
    let db = Database::open_in_memory().unwrap();
    scope(&db);
    answer(&db, 10, "first");
    let h = answer(&db, 30, ""); // Non-output rows still position input.
    receipt(&db, "middle", 1, 25, "delivered", "middle");
    receipt(&db, "tail", 2, 40, "delivered", "tail");
    answer(&db, 20, "later");
    answer(&db, 50, "new");
    let snapshot = db.history_snapshot("thread", None).unwrap().unwrap();
    let p = prepared(&snapshot, h, Direction::Forward);
    assert_eq!(
        words(p.page(Direction::Forward, None, 20, 768 * 1024).unwrap()),
        vec![
            (0, Role::Assistant, "first".into()),
            (1, Role::User, "middle".into()),
            (2, Role::User, "tail".into())
        ]
    );
    let p = prepared(&snapshot, snapshot.latest, Direction::Forward);
    assert_eq!(
        words(p.page(Direction::Forward, None, 20, 768 * 1024).unwrap()),
        vec![
            (0, Role::Assistant, "first".into()),
            (1, Role::User, "middle".into()),
            (2, Role::Assistant, "later".into()),
            (3, Role::User, "tail".into()),
            (4, Role::Assistant, "new".into())
        ]
    );
}

#[test]
fn freeze_is_append_stable_but_detects_receipt_replacement_and_hole_reuse() {
    let db = Database::open_in_memory().unwrap();
    scope(&db);
    answer(&db, 10, "first");
    receipt(&db, "input", 1, 11, "delivered", "same");
    let h = event(
        &db,
        20,
        MessageEvent::MessageDelta {
            run_id: Some("run".into()),
            delta: "delta".into(),
        },
    );
    let key = {
        let s = db.history_snapshot("thread", None).unwrap().unwrap();
        prepared(&s, h, Direction::Forward).freeze
    };
    answer(&db, 30, "after H");
    {
        let s = db.history_snapshot("thread", None).unwrap().unwrap();
        assert_eq!(prepared(&s, h, Direction::Forward).freeze, key);
    }
    db.conn
        .execute(
            "UPDATE host_input_receipts SET text='size' WHERE prompt_id='input'",
            [],
        )
        .unwrap();
    {
        let s = db.history_snapshot("thread", None).unwrap().unwrap();
        assert_ne!(prepared(&s, h, Direction::Forward).freeze, key);
    }
    db.conn
        .execute(
            "UPDATE host_input_receipts SET text='same' WHERE prompt_id='input'",
            [],
        )
        .unwrap();
    // Delete the later committed row and rebuild first, then reuse the noncommitted cut.
    db.conn
        .execute("DELETE FROM events WHERE id>?1", [h])
        .unwrap();
    struct Clock;
    impl super::build::Clock for Clock {
        fn now_ns(&self) -> i64 {
            0
        }
    }
    for _ in 0..8 {
        if db
            .history_build_step("run", Default::default(), &Clock)
            .unwrap()
            .ready
        {
            break;
        }
    }
    let key = {
        let s = db.history_snapshot("thread", None).unwrap().unwrap();
        prepared(&s, h, Direction::Forward).freeze
    };
    db.conn
        .execute("DELETE FROM events WHERE id=?1", [h])
        .unwrap();
    // C28 retains the indexed maximum even through deletion; use a second run's H hole.
    let payload = serde_json::to_string(&EventKind::from(MessageEvent::MessageCompleted {
        run_id: "new".into(),
        text: "new".into(),
    }))
    .unwrap();
    db.conn
        .execute(
            "INSERT INTO thread_runs VALUES ('new','thread','new',0)",
            [],
        )
        .unwrap();
    let key_without_new = {
        let s = db.history_snapshot("thread", None).unwrap().unwrap();
        prepared(&s, h, Direction::Forward).freeze
    };
    db.conn.execute("INSERT INTO events(id,schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload) VALUES (?1,1,0,20,'Message','MessageCompleted','new',?2)",params![h,payload]).unwrap();
    let stored = super::index::state(&db.conn, "new").unwrap().unwrap();
    assert_eq!(stored.phase, 2);
    assert_eq!(stored.generation, Some(0));
    super::index::append_hook(
        &db.conn,
        h,
        20,
        super::index::Representation::Original(&serde_json::from_str(&payload).unwrap()),
    );
    let s = db.history_snapshot("thread", None).unwrap().unwrap();
    assert_ne!(prepared(&s, h, Direction::Forward).freeze, key_without_new);
    assert_ne!(prepared(&s, h, Direction::Forward).freeze, key);
}

#[test]
fn forwards_and_backwards_exclusive_keys_cover_each_item_exactly_once() {
    let db = Database::open_in_memory().unwrap();
    scope(&db);
    for i in 0..7 {
        answer(&db, i + 10, &format!("answer{i}"));
    }
    receipt(&db, "input-a", 1, 12, "delivered", "a");
    receipt(&db, "input-b", 2, 12, "delivered", "b");
    let s = db.history_snapshot("thread", None).unwrap().unwrap();
    let p = prepared(&s, s.latest, Direction::Forward);
    let all = p
        .page(Direction::Forward, None, 99, 768 * 1024)
        .unwrap()
        .items;
    for direction in [Direction::Forward, Direction::Backward] {
        let p = prepared(&s, s.latest, direction);
        let mut boundary = None;
        let mut actual = vec![];
        loop {
            let page = p.page(direction, boundary.as_ref(), 2, 768 * 1024).unwrap();
            assert!(!page.oversized);
            if direction == Direction::Backward {
                actual.splice(0..0, page.items);
            } else {
                actual.extend(page.items);
            }
            boundary = page.next;
            if boundary.is_none() {
                break;
            }
        }
        assert_eq!(actual, all);
    }
}

#[test]
fn legacy_order_of_trailing_input_at_maximum_event_id_is_preserved() {
    let db = Database::open_in_memory().unwrap();
    scope(&db);
    let kind = EventKind::from(MessageEvent::MessageCompleted {
        run_id: "run".into(),
        text: "answer".into(),
    });
    let payload = serde_json::to_string(&kind).unwrap();
    db.conn.execute("INSERT INTO events(id,schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload) VALUES (9223372036854775807,1,0,10,'Message','MessageCompleted','run',?1)",[payload]).unwrap();
    super::index::append_hook(
        &db.conn,
        i64::MAX,
        10,
        super::index::Representation::Original(&kind),
    );
    receipt(&db, "tail", 1, 20, "delivered", "tail");
    let s = db.history_snapshot("thread", None).unwrap().unwrap();
    for direction in [Direction::Forward, Direction::Backward] {
        let p = prepared(&s, s.latest, direction);
        assert_eq!(
            words(p.page(direction, None, 50, 768 * 1024).unwrap()),
            vec![
                (1, Role::User, "tail".into()),
                (0, Role::Assistant, "answer".into())
            ]
        );
    }
}

#[cfg(feature = "test-support")]
#[test]
fn indexed_header_and_source_work_is_page_proportional_and_survives_reopen() {
    let mut samples = vec![];
    for n in [100, 1000] {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::StorageConfig {
            db_path: dir.path().join("page.db"),
            ..Default::default()
        };
        let db = Database::open(&config).unwrap();
        scope(&db);
        for i in 0..n {
            answer(&db, i + 10, "answer");
        }
        drop(db);
        let db = Database::open(&config).unwrap();
        let meter = crate::test_support::meter_history(&db);
        let s = db.history_snapshot("thread", None).unwrap().unwrap();
        let p = prepared(&s, s.latest, Direction::Backward);
        let page = p.page(Direction::Backward, None, 7, 768 * 1024).unwrap();
        assert_eq!(page.items.len(), 7);
        let counts = meter.counts();
        assert_eq!(counts.payload_decodes, 7);
        assert_eq!(counts.source_references, 7);
        assert_eq!(counts.fullscan_steps, 0);
        // The retained receipt SQL opens one empty sorter (the disclosed M cost).
        assert_eq!(counts.sorts, 1);
        assert!(counts.message_headers <= 9);
        samples.push(counts);
    }
    assert_eq!(samples[0].message_headers, samples[1].message_headers);
    assert!(samples[1].vm_steps <= samples[0].vm_steps + 64);
}

#[test]
fn header_byte_caps_match_raw_serialization_including_index_digit_growth() {
    let db = Database::open_in_memory().unwrap();
    scope(&db);
    receipt(&db, "input", 1, 0, "delivered", "input");
    for i in 0..12 {
        answer(&db, i + 10, "answer");
    }
    let snapshot = db.history_snapshot("thread", None).unwrap().unwrap();
    for direction in [Direction::Forward, Direction::Backward] {
        let p = prepared(&snapshot, snapshot.latest, direction);
        let all = p.page(direction, None, 100, usize::MAX).unwrap().items;
        let first = if direction == Direction::Forward {
            &all[0]
        } else {
            all.last().unwrap()
        };
        let bytes = 256 + serde_json::to_vec(first).unwrap().len() + 1;
        let page = p.page(direction, None, 100, bytes).unwrap();
        assert!(!page.oversized);
        assert_eq!(page.items, vec![first.clone()]);
        assert!(page.next.is_some());
        assert!(p.page(direction, None, 100, bytes - 1).unwrap().oversized);
    }
}

#[cfg(feature = "test-support")]
#[test]
fn large_inputs_keep_only_references_and_do_not_read_full_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::StorageConfig {
        db_path: dir.path().join("input.db"),
        ..Default::default()
    };
    let db = Database::open(&config).unwrap();
    scope(&db);
    receipt(&db, "small", 1, 0, "delivered", &"s".repeat(16384));
    receipt(&db, "large", 2, 1, "delivered", &"l".repeat(16385));
    let bodies = crate::test_support::meter_full_body_reads(&db);
    let meter = crate::test_support::meter_history(&db);
    let s = db.history_snapshot("thread", None).unwrap().unwrap();
    let p = prepared(&s, s.latest, Direction::Forward);
    assert_eq!(meter.counts().fingerprint_bytes, 16384);
    let page = p.page(Direction::Forward, None, 50, 768 * 1024).unwrap();
    assert_eq!(page.items[0].parts[0].0.len(), 16384);
    assert!(page.items[0].input.is_none());
    assert_eq!(
        page.items[1].input,
        Some(InputBody {
            id: "large".into(),
            bytes: 16385
        })
    );
    assert!(page.items[1].parts.is_empty());
    assert_eq!(bodies.counts(), Default::default());
}

#[test]
fn page_seek_query_plans_use_composite_keys_without_event_scans_or_sorts() {
    let db = Database::open_in_memory().unwrap();
    scope(&db);
    answer(&db, 10, "answer");
    for sql in [
        "SELECT output_event_id,ordinal,role,encoded_len FROM history_messages WHERE run_id=?1 AND output_event_id>?2 AND output_event_id<=?3 ORDER BY output_event_id LIMIT 1",
        "SELECT event_id,after_count,merge_max FROM history_event_cuts WHERE run_id=?1 AND event_id>=?2 AND event_id<=?3 ORDER BY event_id DESC LIMIT 1",
        "SELECT event_id,before_count FROM history_event_cuts WHERE run_id=?1 AND merge_max>=?2 AND event_id<=?3 ORDER BY merge_max,event_id LIMIT 1",
        "SELECT event_id FROM history_message_sources WHERE run_id=?1 AND output_event_id=?2 AND part_index<?3 ORDER BY part_index",
    ] {
        let plans: Vec<String> = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(rusqlite::params!["run", 0i64, i64::MAX], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plans.iter().any(|p| p.contains("SEARCH history_")),
            "{plans:?}"
        );
        assert!(
            plans
                .iter()
                .all(|p| !p.contains("SCAN history_") && !p.contains("TEMP B-TREE")),
            "{plans:?}"
        );
    }
}
