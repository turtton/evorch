use super::*;
use crate::repo::event::{EventAccounting, append_event, list_all_ordered};
use crate::thread_retention::{BindingInput, NANOS_PER_DAY};
use crate::{Database, HardLimits};
use event_bus::{Event, EventKind, EventMeta, ToolEvent};
use serde_json::json;
use std::time::{Duration, UNIX_EPOCH};

fn fixture() -> (Database, Request) {
    let db = Database::open_in_memory().unwrap();
    let c = &db.conn;
    c.execute_batch("INSERT INTO sessions(id,status,created_at_ns,updated_at_ns) VALUES('s','running',0,0); INSERT INTO threads(thread_id,root_run_id,created_at_ns,updated_at_ns,source) VALUES('t','r',0,0,'host'); INSERT INTO thread_runs VALUES('r','t','r',0); INSERT INTO retention_binding VALUES(1,'root','sidebar',0);").unwrap();
    let request = Request {
        db_identity: db.retention_identity().unwrap(),
        binding: BindingInput {
            ownership_root: "root".into(),
            sidebar_path: "sidebar".into(),
        },
        expected: db.tool_body_canonical("t").unwrap().unwrap(),
        run_id: "r".into(),
        after_event: 0,
        now_ns: 8 * NANOS_PER_DAY,
        after_days: 7,
    };
    (db, request)
}
fn add(db: &Database, kind: impl Into<EventKind>) -> i64 {
    let event = Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::ZERO,
            wall_clock: UNIX_EPOCH,
        },
        kind: kind.into(),
    };
    append_event(
        &db.conn,
        Some("s"),
        &event,
        // Exercise retention caps independently from the configurable append cap.
        &HardLimits {
            max_event_bytes: 8 * 1024 * 1024,
            ..Default::default()
        },
        &mut EventAccounting::default(),
    )
    .unwrap()
    .id
}
fn completed(output: String, detail: Option<Value>) -> ToolEvent {
    ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "call".into(),
        is_error: true,
        output: Some(output),
        detail,
        run_id: Some("r".into()),
        parent_call_id: Some("parent".into()),
    }
}
fn unit(db: &Database, request: &Request) -> Progress {
    let tx =
        rusqlite::Transaction::new_unchecked(&db.conn, rusqlite::TransactionBehavior::Immediate)
            .unwrap();
    let p = apply(&tx, request).unwrap();
    tx.commit().unwrap();
    p
}
fn count(db: &Database, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn tool_body_retention_exact_fields_boundaries_escape_and_gc() {
    let (db, request) = fixture();
    // Short escape token represents literal token-looking input, not a long body.
    let escape = BodyToken::of("literal").to_value_string();
    let long = "あ".repeat(683); // UTF-8 2049, not character count.
    let start = ToolEvent::ToolStarted {
        tool_name: "tool".into(),
        call_id: "start".into(),
        input: Some(
            json!({"nested":["x".repeat(2047),"y".repeat(2048),escape.clone(),long.clone()]}),
        ),
        run_id: Some("r".into()),
        parent_call_id: None,
    };
    add(&db, start);
    add(
        &db,
        completed(
            "out".repeat(1000),
            Some(json!({"short":"kept","long":long.clone()})),
        ),
    );
    // Same content in a non-target Message remains referenced, including after GC.
    add(
        &db,
        event_bus::MessageEvent::MessageCompleted {
            run_id: "r".into(),
            text: long.clone(),
        },
    );
    let before = list_all_ordered(&db.conn).unwrap();
    let p = unit(&db, &request);
    assert_eq!(p.changed, 2);
    let after = list_all_ordered(&db.conn).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before[2], after[2]);
    assert_eq!(before[0].id, after[0].id);
    assert_eq!(before[0].event.meta, after[0].event.meta);
    let EventKind::Tool(ToolEvent::ToolStarted {
        input: Some(input),
        tool_name,
        call_id,
        ..
    }) = &after[0].event.kind
    else {
        panic!()
    };
    assert_eq!(tool_name, "tool");
    assert_eq!(call_id, "start");
    assert_eq!(
        input["nested"],
        json!(["x".repeat(2047), REMOVED, escape, REMOVED])
    );
    let EventKind::Tool(ToolEvent::ToolCompleted {
        output,
        detail,
        is_error,
        parent_call_id,
        ..
    }) = &after[1].event.kind
    else {
        panic!()
    };
    assert_eq!(output.as_deref(), Some(REMOVED));
    assert!(*is_error);
    assert_eq!(parent_call_id.as_deref(), Some("parent"));
    assert_eq!(detail, &Some(json!({"short":"kept","long":REMOVED})));
    assert_eq!(
        count(&db, "SELECT count(*) FROM events WHERE body_format=1"),
        3
    );
    let gc = crate::repo::content_blobs::gc_step(&db.conn).unwrap();
    assert!(gc.deleted >= 2);
    assert!(gc.retained >= 1);
    assert_eq!(list_all_ordered(&db.conn).unwrap(), after);
    assert_eq!(unit(&db, &request).changed, 0);
    assert_eq!(
        count(&db, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
}
#[test]
fn tool_body_retention_rollback_keeps_payload_refs_and_queue() {
    let (db, request) = fixture();
    add(&db, completed("x".repeat(2048), None));
    let before = list_all_ordered(&db.conn).unwrap();
    let refs = count(&db, "SELECT count(*) FROM content_blob_refs_event");
    {
        let tx = rusqlite::Transaction::new_unchecked(
            &db.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        assert_eq!(apply(&tx, &request).unwrap().changed, 1);
    }
    assert_eq!(list_all_ordered(&db.conn).unwrap(), before);
    assert_eq!(
        count(&db, "SELECT count(*) FROM content_blob_refs_event"),
        refs
    );
    assert_eq!(count(&db, "SELECT count(*) FROM content_blob_gc_queue"), 0);
}
#[test]
fn tool_body_retention_deadline_membership_pin_and_source_changes_hold() {
    for change in [
        "UPDATE threads SET pinned=1",
        "UPDATE threads SET pin_revision=2",
        "INSERT INTO thread_runs VALUES('child','t','r',1)",
        "UPDATE threads SET created_at_ns=42",
        "INSERT INTO thread_start_states(thread_id,root_run_id,project_root,repo_prefix,head_commit,diff,captured_at_ns,replayable) VALUES('t','r','/fixture','','fixture',X'',0,1); INSERT INTO eval_samples(thread_id,problem,focus_roles,created_at_ns,updated_at_ns) VALUES('t','p','[]',0,0)",
    ] {
        let (db, request) = fixture();
        add(&db, completed("x".repeat(2048), None));
        db.conn.execute_batch(change).unwrap();
        let p = unit(&db, &request);
        assert_eq!(p.changed, 0, "{change}");
        assert!(p.hold.is_some(), "{change}");
    }
    assert_eq!(cutoff(7 * NANOS_PER_DAY, 7), Some(0));
    assert_eq!(cutoff(0, 7), None);
    assert_eq!(cutoff(i64::MAX, u32::MAX), None);
    assert_eq!(cutoff(8 * NANOS_PER_DAY, 0), None);
    let (db, mut r) = fixture();
    add(&db, completed("x".repeat(2048), None));
    r.now_ns = 7 * NANOS_PER_DAY;
    assert_eq!(unit(&db, &r).changed, 1, "activity at cutoff is due");
}
#[test]
fn tool_body_retention_canonical_limit_and_fork_ownership_are_bounded() {
    let (db, mut request) = fixture();
    add(&db, completed("x".repeat(2048), None));
    for n in 1..1025 {
        db.conn
            .execute(
                "INSERT INTO thread_runs VALUES(?1,'t','r',1)",
                [format!("r{n}")],
            )
            .unwrap();
    }
    request.expected = db.tool_body_canonical("t").unwrap().unwrap();
    assert_eq!(request.expected.runs.len(), 1025);
    assert_eq!(unit(&db, &request).hold, Some("CanonicalRunBudget"));
    db.conn
        .execute("DELETE FROM thread_runs WHERE run_id!='r'", [])
        .unwrap();
    db.conn.execute_batch("INSERT INTO threads(thread_id,root_run_id,parent_thread_id,lineage_kind,created_at_ns,updated_at_ns,source,pinned) VALUES('fork','f','t','fork',0,0,'host',1); INSERT INTO thread_runs VALUES('f','fork','f',0);").unwrap();
    assert!(
        db.conn
            .execute("INSERT INTO thread_runs VALUES('r','fork','f',1)", [])
            .is_err(),
        "canonical run belongs to one thread"
    );
    request.expected = db.tool_body_canonical("t").unwrap().unwrap();
    assert_eq!(
        unit(&db, &request).changed,
        1,
        "pinned fork is not a deletion family"
    );
}
#[test]
fn tool_body_retention_row_decode_reference_budgets_and_corruption_progress() {
    let (db, mut request) = fixture();
    for n in 0..800 {
        add(&db, completed(format!("{n}:{}", "x".repeat(2048)), None));
    }
    let mut units = 0;
    let mut changed = 0;
    loop {
        let p = unit(&db, &request);
        assert!(p.inspected <= EVENT_ROWS);
        assert!(p.stored_bytes <= SKELETON_BYTES);
        assert!(p.decoded_bytes <= SKELETON_BYTES);
        assert!(p.old_refs <= REFERENCE_WINDOW);
        assert!(p.new_refs <= REFERENCE_WINDOW);
        units += 1;
        changed += p.changed;
        request.after_event = p.after_event;
        if p.done {
            break;
        }
    }
    assert_eq!(changed, 800);
    assert_eq!(units, 4);
    let id = add(&db, completed("z".repeat(2048), None));
    db.conn
        .execute("UPDATE events SET payload=x'00' WHERE id=?1", [id])
        .unwrap();
    add(&db, completed("a".repeat(2048), None));
    let p = unit(&db, &request);
    assert_eq!(p.after_event, id);
    request.after_event = p.after_event;
    assert_eq!(
        unit(&db, &request).changed,
        1,
        "corrupt row does not starve following rows"
    );
}
#[test]
fn tool_body_retention_reference_first_cursor_wrap_and_restart_reaches_tail() {
    let (db, _) = fixture();
    for _ in 0..300 {
        add(
            &db,
            event_bus::MessageEvent::MessageCompleted {
                run_id: "r".into(),
                text: "non-target".repeat(256),
            },
        );
    }
    add(&db, completed("target".repeat(512), None));
    for restart in [0, 5, 111, 300] {
        let mut cursor = Cursor::default();
        let mut found = false;
        for _ in 0..8 {
            if let Some(c) = db.tool_body_candidate(&mut cursor, restart).unwrap() {
                assert_eq!(c.thread_id, "t");
                found = true;
                break;
            }
        }
        assert!(found, "restart seed {restart}");
    }
}

struct At;
impl crate::history::build::Clock for At {
    fn now_ns(&self) -> i64 {
        0
    }
}
fn build(db: &Database) -> usize {
    let mut writes = 0;
    // The existing builder intentionally applies at most one committed source
    // per step, even though its source-row allowance is larger.
    for _ in 0..(count(db, "SELECT count(*) FROM events") + 16) {
        let p = crate::history::build::step(db, db, "r", Default::default(), &At).unwrap();
        writes += p.writes;
        if p.ready {
            return writes;
        }
    }
    panic!(
        "finite history fixture did not finish: {:?}",
        db.conn
            .query_row(
                "SELECT generation,source,phase,checkpoint FROM history_run_state WHERE run_id='r'",
                [],
                |r| Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?
                ))
            )
            .unwrap()
    )
}
#[test]
fn tool_body_retention_history_rebuilds_at_most_once_per_changed_unit() {
    for rows in [256_usize, 800] {
        let (db, mut request) = fixture();
        for n in 0..rows {
            add(&db, completed(format!("{n}:{}", "x".repeat(2048)), None));
        }
        // Force baseline construction, as if the thread had already been opened.
        db.conn
            .execute("UPDATE history_run_state SET phase=0", [])
            .unwrap();
        let baseline = build(&db);
        let derived: usize = crate::history::build::TABLES
            .iter()
            .map(|table| count(&db, &format!("SELECT count(*) FROM {table}")) as usize)
            .sum();
        let w = baseline + derived + 32;
        let mut units = 0;
        let mut rebuilds = 0;
        let mut writes = 0;
        loop {
            let p = unit(&db, &request);
            if p.changed != 0 {
                units += 1;
                assert_eq!(
                    count(&db, "SELECT phase FROM history_run_state WHERE run_id='r'"),
                    0
                );
                rebuilds += 1;
                writes += build(&db);
            }
            request.after_event = p.after_event;
            if p.done {
                break;
            }
        }
        assert_eq!(units, rows.div_ceil(256));
        assert!(rebuilds <= units);
        assert!(writes <= units * w);
        let generation = count(
            &db,
            "SELECT generation FROM history_run_state WHERE run_id='r'",
        );
        request.after_event = 0;
        assert_eq!(unit(&db, &request).changed, 0);
        assert_eq!(
            count(
                &db,
                "SELECT generation FROM history_run_state WHERE run_id='r'"
            ),
            generation
        );
        println!("rows={rows} U={units} rebuilds={rebuilds} writes={writes} W_bound={w}");
    }
}
#[test]
fn tool_body_retention_rejects_prepared_old_history_generation() {
    let (db, request) = fixture();
    add(&db, completed("x".repeat(2048), None));
    db.conn
        .execute("UPDATE history_run_state SET phase=0", [])
        .unwrap();
    let budget = crate::history::build::BuildBudget::default();
    let progress = crate::history::build::start(&db.conn, "r", budget).unwrap();
    let batch = crate::history::build::prepare(&db, "r", budget, &At, progress)
        .unwrap()
        .unwrap();
    assert_eq!(unit(&db, &request).changed, 1);
    assert!(
        crate::history::build::apply(&db.conn, batch)
            .unwrap()
            .conflicted
    );
    build(&db);
}
#[test]
fn tool_body_retention_sql_uses_indexed_source_windows() {
    let (db, _) = fixture();
    for (sql, needle) in [
        (
            "EXPLAIN QUERY PLAN SELECT event_id,blob_id FROM content_blob_refs_event WHERE (event_id,blob_id)>(1,1) ORDER BY event_id,blob_id LIMIT 2000",
            "SEARCH content_blob_refs_event",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT id,kind,subkind FROM events INDEXED BY idx_events_run_id WHERE run_id='r' AND id>0 ORDER BY id LIMIT 256",
            "SEARCH events USING INDEX idx_events_run_id",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT run_id FROM thread_runs INDEXED BY idx_thread_runs_thread WHERE thread_id='t' LIMIT 1025",
            "SEARCH thread_runs USING INDEX idx_thread_runs_thread",
        ),
    ] {
        let plans: Vec<String> = db
            .conn
            .prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(plans.iter().any(|p| p.contains(needle)), "{plans:?}");
        assert!(
            !plans.iter().any(|p| p.contains("TEMP B-TREE")),
            "{plans:?}"
        );
    }
}

#[test]
fn tool_body_retention_question_and_existing_lane_protections() {
    for (own, root, link) in [
        ("r", "other", false),
        ("other", "r", false),
        ("other", "other", true),
    ] {
        let (db, request) = fixture();
        add(&db, completed("x".repeat(2048), None));
        db.conn.execute("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES('q',?1,?2,'root','{}')",params![own,root]).unwrap();
        if link {
            db.conn
                .execute("INSERT INTO user_question_links VALUES('q','r')", [])
                .unwrap();
        }
        assert_eq!(unit(&db, &request).hold, Some("UnansweredQuestion"));
        db.conn
            .execute("UPDATE user_questions SET answered=1", [])
            .unwrap();
        assert_eq!(unit(&db, &request).changed, 1);
    }
    for (policy, closed, protected) in [
        ("attached", false, true),
        ("summary", false, true),
        ("attached", true, false),
        ("detached", false, false),
    ] {
        let (db, request) = fixture();
        add(&db, completed("x".repeat(2048), None));
        let brief=json!({"objective":"o","inputs":[],"deliverables":[],"acceptance_criteria":[],"constraints":[],"report_policy":policy}).to_string();
        let lead = (policy != "detached").then_some("lead");
        db.conn.execute("INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,request_hash,scope,lead_thread_id,policy,management_revision,management_projection_revision,brief_json,brief_revision,start_seed,root_run_id,generation,created_at_ns,updated_at_ns,last_activity_at_ns) VALUES('lane','t','lead','r0','c','h','lane',?1,?2,1,0,?3,1,'{}','r',1,0,0,0)",params![lead,policy,brief]).unwrap();
        if closed {
            db.conn.execute_batch("INSERT INTO lane_closes(close_id,lane_id,kind,payload) VALUES('close','lane','human_closed','{}'); UPDATE lanes SET current_close_id='close';").unwrap();
        }
        assert_eq!(
            unit(&db, &request).changed,
            if protected { 0 } else { 1 },
            "{policy} closed={closed}"
        );
    }
}
#[test]
fn tool_body_retention_ignores_goal_but_not_recent_input_or_activity() {
    let (db, request) = fixture();
    add(&db, completed("x".repeat(2048), None));
    // A goal is not decoded at all (even an old undecodable goal does not hold).
    db.conn.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Orchestrator','ThreadGoalUpdated','r',0,'not a goal')",[]).unwrap();
    assert_eq!(unit(&db, &request).changed, 1);
    let (db, request) = fixture();
    add(&db, completed("x".repeat(2048), None));
    let event = Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::ZERO,
            wall_clock: UNIX_EPOCH + Duration::from_nanos(request.now_ns as u64),
        },
        kind: event_bus::MessageEvent::MessageCompleted {
            run_id: "r".into(),
            text: "recent".into(),
        }
        .into(),
    };
    append_event(
        &db.conn,
        Some("s"),
        &event,
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap();
    assert_eq!(unit(&db, &request).hold, Some("RecentActivity"));
}

#[test]
fn tool_body_retention_stored_decoded_and_reference_caps_skip_without_losing_progress() {
    for compressed in [false, true] {
        let (db, request) = fixture();
        let oversized = add(&db, completed("old".repeat(1000), None));
        let raw: String = db
            .conn
            .query_row("SELECT payload FROM events WHERE id=?1", [oversized], |r| {
                r.get(0)
            })
            .unwrap();
        let mut doc: Value = serde_json::from_str(&raw).unwrap();
        doc["unknown_extension"] = Value::String("x".repeat(SKELETON_BYTES));
        let raw = serde_json::to_string(&doc).unwrap();
        let stored = if compressed {
            SqlValue::Blob(zstd::stream::encode_all(raw.as_bytes(), 3).unwrap())
        } else {
            SqlValue::Text(raw)
        };
        db.conn
            .execute(
                "UPDATE events SET payload=?1 WHERE id=?2",
                params![stored.clone(), oversized],
            )
            .unwrap();
        let next = add(&db, completed("next".repeat(1000), None));
        let p = unit(&db, &request);
        assert!(p.decoded_bytes <= SKELETON_BYTES);
        let mut following = request.clone();
        following.after_event = p.after_event;
        let next_unit = unit(&db, &following);
        assert_eq!(p.changed + next_unit.changed, 1, "compressed={compressed}");
        assert!(next_unit.after_event >= next);
        let after: SqlValue = db
            .conn
            .query_row("SELECT payload FROM events WHERE id=?1", [oversized], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, after);
        assert_eq!(
            count(&db, "SELECT count(*) FROM content_blob_refs_event"),
            1
        );
    }
    let (db, request) = fixture();
    let oversized = add(
        &db,
        ToolEvent::ToolStarted {
            tool_name: "many".into(),
            call_id: "many".into(),
            input: Some(Value::Array(
                (0..=REFERENCE_WINDOW)
                    .map(|n| Value::String(format!("{n:05}{}", "x".repeat(2048))))
                    .collect(),
            )),
            run_id: Some("r".into()),
            parent_call_id: None,
        },
    );
    let before: SqlValue = db
        .conn
        .query_row("SELECT payload FROM events WHERE id=?1", [oversized], |r| {
            r.get(0)
        })
        .unwrap();
    add(&db, completed("next".repeat(1000), None));
    let p = unit(&db, &request);
    assert_eq!(p.changed, 1);
    let after: SqlValue = db
        .conn
        .query_row("SELECT payload FROM events WHERE id=?1", [oversized], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        count(&db, "SELECT count(*) FROM content_blob_refs_event"),
        2001
    );
}
