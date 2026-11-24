use super::index::*;
use crate::repo::event::{EventAccounting, append_event};
use crate::{Database, HardLimits};
use event_bus::{Event, EventKind, MessageEvent};

fn append(db: &Database, kind: impl Into<EventKind>) -> i64 {
    append_event(
        &db.conn,
        None,
        &Event::new(kind),
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap()
    .id
}
fn message(run: &str, text: &str) -> MessageEvent {
    MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: text.into(),
    }
}
fn int(db: &Database, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn new_run_indexes_each_committed_cut_and_only_displayed_outputs() {
    let db = Database::open_in_memory().unwrap();
    append(&db, message("run", "answer"));
    append(
        &db,
        MessageEvent::FinalResultPublished {
            run_id: "run".into(),
            text: "answer".into(),
        },
    );
    append(&db, message("run", ""));
    let state = state(&db.conn, "run").unwrap().unwrap();
    assert_eq!(state.phase, 2);
    assert_eq!(state.source, state.applied);
    assert_eq!(state.reducer.next, 1);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_event_cuts"), 3);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_messages"), 1);
}

#[test]
fn pending_parts_are_references_and_normalize_only_when_complete() {
    let db = Database::open_in_memory().unwrap();
    for (index, text) in ["a", "b", "c"].into_iter().enumerate() {
        append(
            &db,
            MessageEvent::MessagePart {
                run_id: "run".into(),
                response_id: "response".into(),
                index,
                total: 3,
                text: text.into(),
            },
        );
        assert_eq!(
            int(&db, "SELECT COUNT(*) FROM history_pending_parts"),
            if index == 2 { 0 } else { index as i64 + 1 }
        );
    }
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_message_sources"), 3);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_messages"), 1);
    append(
        &db,
        MessageEvent::FinalResultPublished {
            run_id: "run".into(),
            text: "abc".into(),
        },
    );
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_messages"), 1);
    let columns: Vec<String> = db
        .conn
        .prepare("SELECT name FROM pragma_table_info('history_pending_parts')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        columns,
        ["run_id", "response_id", "part_index", "total", "event_id"]
    );
}

#[test]
fn failed_projection_rolls_back_derived_rows_but_not_event_or_outer_transaction() {
    let db = Database::open_in_memory().unwrap();
    db.conn.execute_batch("CREATE TABLE outer_write(value INTEGER); CREATE TRIGGER history_fault BEFORE INSERT ON history_messages BEGIN SELECT RAISE(FAIL,'injected projection failure'); END;").unwrap();
    let tx = db.conn.unchecked_transaction().unwrap();
    crate::repo::event::append_event_in_transaction(
        &tx,
        None,
        &Event::new(message("run", "answer")),
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap();
    tx.execute("INSERT INTO outer_write VALUES (1)", [])
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(int(&db, "SELECT COUNT(*) FROM events"), 1);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM outer_write"), 1);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_messages"), 0);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_event_cuts"), 0);
    let state = state(&db.conn, "run").unwrap().unwrap();
    assert_ne!(state.source, state.applied);
    assert_eq!(state.checkpoint, None);
}

#[test]
fn direct_sql_mutations_invalidate_old_and_new_runs() {
    let db = Database::open_in_memory().unwrap();
    let id = append(&db, message("a", "answer"));
    append(&db, message("b", "other"));
    db.conn
        .execute("UPDATE events SET run_id='b' WHERE id=?1", [id])
        .unwrap();
    for run in ["a", "b"] {
        let state = state(&db.conn, run).unwrap().unwrap();
        assert_eq!(state.phase, 0);
        assert!(state.generation.unwrap() > 0);
    }
    let last = state(&db.conn, "a").unwrap().unwrap().last_indexed;
    db.conn
        .execute("DELETE FROM events WHERE id=?1", [id])
        .unwrap();
    assert_eq!(state(&db.conn, "a").unwrap().unwrap().last_indexed, last);
}

#[test]
fn noncommitted_tail_id_reuse_does_not_dirty_the_projection() {
    let db = Database::open_in_memory().unwrap();
    append(&db, message("run", "first"));
    let tail = append(
        &db,
        MessageEvent::MessageDelta {
            run_id: Some("run".into()),
            delta: "delta".into(),
        },
    );
    db.conn
        .execute("DELETE FROM events WHERE id=?1", [tail])
        .unwrap();
    assert_eq!(append(&db, message("run", "second")), tail);
    let state = state(&db.conn, "run").unwrap().unwrap();
    assert_eq!(state.phase, 2);
    assert_eq!(state.generation, Some(0));
    assert_eq!(state.source, state.applied);
    assert_eq!(state.reducer.next, 2);
}

#[test]
fn newer_projection_version_is_not_downgraded_by_append() {
    let db = Database::open_in_memory().unwrap();
    let newer = REDUCER_VERSION + 1;
    db.conn
        .execute("UPDATE history_control SET reducer_version=?1", [newer])
        .unwrap();
    append(&db, message("run", "answer"));
    assert!(!version_ok(&db.conn).unwrap());
    assert_eq!(
        int(&db, "SELECT reducer_version FROM history_control"),
        newer
    );
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_messages"), 0);
}

#[test]
fn append_after_4096_deferred_parts_keeps_fixed_size_reducer_and_no_source_reads() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&crate::StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    })
    .unwrap();
    for index in 0..4096 {
        append(
            &db,
            MessageEvent::MessagePart {
                run_id: "run".into(),
                response_id: "response".into(),
                index,
                total: 4096,
                text: format!("\u{e000}part-{index}"),
            },
        );
    }
    assert_eq!(
        int(&db, "SELECT COUNT(*) FROM history_message_sources"),
        4096
    );
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_pending_parts"), 0);
    assert!(int(&db, "SELECT octet_length(reducer) FROM history_run_state") < 512);
    let meter = crate::test_support::meter_history(&db);
    append(
        &db,
        event_bus::ToolEvent::ToolCompleted {
            run_id: Some("run".into()),
            call_id: "tool".into(),
            tool_name: "test".into(),
            is_error: false,
            output: None,
            detail: None,
            parent_call_id: None,
        },
    );
    assert_eq!(meter.counts().payload_decodes, 0);
    assert_eq!(meter.counts().source_rows, 0);
    let state = state(&db.conn, "run").unwrap().unwrap();
    assert_eq!(state.phase, 2);
    assert_eq!(state.source, state.applied);
    assert_eq!(state.reducer.next, 1);
}

#[test]
fn unwinding_after_derived_writes_keeps_the_source_and_rolls_back_the_projection() {
    let db = Database::open_in_memory().unwrap();
    PANIC_AFTER_APPLY.with(|fault| fault.set(true));
    append(&db, message("run", "answer"));
    assert_eq!(int(&db, "SELECT COUNT(*) FROM events"), 1);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_messages"), 0);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_event_cuts"), 0);
    let state = state(&db.conn, "run").unwrap().unwrap();
    assert_ne!(state.source, state.applied);
    assert_eq!(state.checkpoint, None);
}

#[test]
fn trigger_classification_matches_committed_subkinds_and_undo_is_registered() {
    let db = Database::open_in_memory().unwrap();
    for (kind, subkinds) in [
        (
            "Message",
            &[
                "MessageCompleted",
                "FinalResultPublished",
                "MessagePart",
                "MessageDelta",
                "ReasoningDelta",
            ][..],
        ),
        (
            "Lifecycle",
            &["TaskPromptPublished", "AgentRunStarted", "AgentRunFinished"][..],
        ),
        (
            "Tool",
            &["ToolCompleted", "ToolStarted", "ToolProgress"][..],
        ),
    ] {
        for subkind in subkinds {
            let run = format!("direct-{kind}-{subkind}");
            db.conn.execute("INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload) VALUES (1,0,0,?1,?2,?3,'{}')",rusqlite::params![kind,subkind,run]).unwrap();
            let state = state(&db.conn, &run).unwrap();
            assert_eq!(
                state.is_some(),
                crate::COMMITTED_SUBKINDS.contains(&(kind, *subkind))
            );
            if let Some(state) = state {
                assert_eq!(state.source, Some(1));
                assert_eq!(state.applied, Some(0));
            }
        }
    }
    let triggers: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'history_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(triggers, 6);
    db.conn
        .execute_batch(include_str!("../../undo/undo_v73.sql"))
        .unwrap();
    assert_eq!(
        int(
            &db,
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'history_%'"
        ),
        0
    );
    crate::migrations::apply_migrations(&db.conn).unwrap();
    assert!(version_ok(&db.conn).unwrap());
}

struct FixedClock;
impl super::build::Clock for FixedClock {
    fn now_ns(&self) -> i64 {
        0
    }
}

fn derived(db: &Database) -> Vec<Vec<String>> {
    [
        "history_event_cuts",
        "history_messages",
        "history_message_sources",
        "history_pending_parts",
    ]
    .map(|table| {
        let mut statement = db
            .conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                Ok((0..columns)
                    .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
    .into()
}

#[test]
fn rebuilding_from_checkpoints_matches_incremental_inline_tokenized_and_part_rows() {
    let db = Database::open_in_memory().unwrap();
    append(&db, message("run", "plain"));
    append(&db, message("run", &"large".repeat(7000)));
    for (index, text) in ["inline", "more", "large"].into_iter().enumerate() {
        append(
            &db,
            MessageEvent::MessagePart {
                run_id: "run".into(),
                response_id: "parts".into(),
                index,
                total: 3,
                text: if index == 2 {
                    text.repeat(7000)
                } else {
                    text.into()
                },
            },
        );
    }
    append(
        &db,
        MessageEvent::MessagePart {
            run_id: "run".into(),
            response_id: "unfinished".into(),
            index: 0,
            total: 2,
            text: "pending".into(),
        },
    );
    let expected = derived(&db);
    let reducer = state(&db.conn, "run").unwrap().unwrap().reducer;
    db.conn
        .execute(
            "UPDATE history_run_state SET phase=0,generation=generation+1 WHERE run_id='run'",
            [],
        )
        .unwrap();
    let budget = super::build::BuildBudget {
        rows: 8,
        writes: 32,
        ..Default::default()
    };
    let mut ready = false;
    for _ in 0..32 {
        let progress = db.history_build_step("run", budget, &FixedClock).unwrap();
        assert!(progress.rows <= budget.rows);
        assert!(progress.writes <= budget.writes);
        if progress.ready {
            ready = true;
            break;
        }
    }
    assert!(ready);
    assert_eq!(derived(&db), expected);
    assert_eq!(
        serde_json::to_value(state(&db.conn, "run").unwrap().unwrap().reducer).unwrap(),
        serde_json::to_value(reducer).unwrap()
    );
}

#[test]
fn a_corrupt_committed_row_cannot_be_skipped_to_ready() {
    let db = Database::open_in_memory().unwrap();
    let id = append(&db, message("run", "plain"));
    db.conn
        .execute("UPDATE events SET payload='invalid' WHERE id=?1", [id])
        .unwrap();
    assert!(
        db.history_build_step("run", Default::default(), &FixedClock)
            .is_err()
    );
    let state = state(&db.conn, "run").unwrap().unwrap();
    assert_eq!(state.phase, 1);
    assert_ne!(state.source, state.applied);
    assert_eq!(state.checkpoint, None);
}

#[test]
fn a_new_runs_empty_proof_does_not_decode_its_noncommitted_prefix() {
    let db = Database::open_in_memory().unwrap();
    db.conn
        .execute_batch(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000)
         INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload)
         SELECT 1,0,0,'Message','MessageDelta','run','unreadable noncommitted' FROM n;",
        )
        .unwrap();
    append(&db, message("run", "first committed"));
    let state = state(&db.conn, "run").unwrap().unwrap();
    assert_eq!(state.phase, 2);
    assert_eq!(state.source, state.applied);
    assert_eq!(state.reducer.next, 1);
}

#[test]
fn generation_exhaustion_never_wraps_or_becomes_ready() {
    let db = Database::open_in_memory().unwrap();
    let id = append(&db, message("run", "plain"));
    db.conn
        .execute(
            "UPDATE history_run_state SET generation=9223372036854775807",
            [],
        )
        .unwrap();
    db.conn
        .execute("UPDATE events SET wall_clock_ns=1 WHERE id=?1", [id])
        .unwrap();
    assert_eq!(state(&db.conn, "run").unwrap().unwrap().generation, None);
    assert!(
        !db.history_build_step("run", Default::default(), &FixedClock)
            .unwrap()
            .ready
    );
}

#[test]
fn empty_checkpoints_do_not_skip_zero_or_minimum_event_ids() {
    let db = Database::open_in_memory().unwrap();
    for id in [i64::MIN, 0] {
        let payload =
            serde_json::to_string(&EventKind::from(message("run", &id.to_string()))).unwrap();
        db.conn.execute(
            "INSERT INTO events(id,schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload)
             VALUES (?1,1,0,?1,'Message','MessageCompleted','run',?2)",
            rusqlite::params![id, payload],
        ).unwrap();
    }
    let mut ready = false;
    for _ in 0..8 {
        if db
            .history_build_step("run", Default::default(), &FixedClock)
            .unwrap()
            .ready
        {
            ready = true;
            break;
        }
    }
    assert!(ready);
    assert_eq!(int(&db, "SELECT COUNT(*) FROM history_messages"), 2);
    assert_eq!(
        int(
            &db,
            "SELECT merge_max FROM history_event_cuts WHERE event_id=-9223372036854775808"
        ),
        -i64::MAX
    );
}
