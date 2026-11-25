//! Contract tests use direct ticks and persisted state, never elapsed time.
use super::*;
use crate::{Storage, StorageConfig};
use event_bus::{MessageEvent, ThreadGoalUsage};
struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
    reader: Database,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("activity.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let reader = Database::open(&config).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
            reader,
        }
    }
    fn insert(&self, kind: EventKind, at: i64) -> i64 {
        self.reader.conn.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,?1,?2,?3,?4,?5)",params![crate::repo::event::kind_name(&kind),kind.subkind_name(),kind.primary_run_id(),at,serde_json::to_string(&kind).unwrap()]).unwrap();
        self.reader.conn.last_insert_rowid()
    }
    fn tick(&self) -> ActivityReport {
        self.storage
            .handle()
            .apply_activity_page(self.reader.activity_page().unwrap())
            .unwrap()
    }
    fn drain(&self) {
        loop {
            let r = self.tick();
            assert!(r.inspected_rows <= ACTIVITY_ROWS_PER_TICK);
            assert!(r.decode_bytes <= ACTIVITY_DECODE_BYTES_PER_TICK);
            if !r.backfill_pending && r.pending_sources == 0 {
                break;
            }
        }
    }
}
fn message(run: &str) -> EventKind {
    MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: "text".into(),
    }
    .into()
}
fn goal(id: &str, thread: &str, root: &str, phase: ThreadGoalPhase) -> ThreadGoalSnapshot {
    ThreadGoalSnapshot {
        goal_id: id.into(),
        thread_id: thread.into(),
        root_run_id: root.into(),
        related_root_run_ids: vec![],
        objective: "objective".into(),
        criteria: vec!["done".into()],
        checks: vec![],
        phase,
        review_enabled: false,
        checks_paused: false,
        work_stopped: false,
        epoch: 0,
        review_round: 0,
        findings: vec![],
        reason: None,
        usage: ThreadGoalUsage::default(),
        max_review_rounds: 1,
        max_tokens: None,
        original_request: "request".into(),
    }
}
fn snapshot(g: ThreadGoalSnapshot) -> EventKind {
    OrchestratorEvent::ThreadGoalUpdated { snapshot: g }.into()
}
#[test]
fn source_commit_survives_derived_rollback_and_reopen() {
    let mut f = Fixture::new();
    f.drain();
    f.reader.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let id = f.insert(message("run"), 100);
    f.reader.conn.execute_batch("SAVEPOINT derived; INSERT INTO retention_activity_runs(run_id,actual_ns) VALUES('run',100); ROLLBACK TO derived; RELEASE derived; COMMIT").unwrap();
    assert!(!f.reader.run_activity("run").unwrap().ready);
    f.reader = Database::open(&f.config).unwrap();
    assert!(!f.reader.run_activity("run").unwrap().ready);
    let stale = f.reader.activity_page().unwrap();
    f.reader
        .conn
        .execute("UPDATE events SET wall_clock_ns=200 WHERE id=?1", [id])
        .unwrap();
    assert!(
        f.storage
            .handle()
            .apply_activity_page(stale)
            .unwrap()
            .cas_rejected
    );
    f.drain();
    let state = f.reader.run_activity("run").unwrap();
    assert!(state.ready);
    assert_eq!(state.last_actual_activity_at, Some(200));
    f.reader
        .conn
        .execute("DELETE FROM events WHERE id=?1", [id])
        .unwrap();
    assert!(!f.reader.run_activity("run").unwrap().ready);
    f.drain();
    assert_eq!(
        f.reader
            .run_activity("run")
            .unwrap()
            .last_actual_activity_at,
        Some(200),
        "deletion cannot lower a known conservative bound"
    );
}
#[test]
fn unrelated_events_have_zero_projection_writes() {
    let f = Fixture::new();
    let id = f.insert(
        MessageEvent::MessageDelta {
            run_id: Some("r".into()),
            delta: "delta".into(),
        }
        .into(),
        1,
    );
    f.reader
        .conn
        .execute("UPDATE events SET wall_clock_ns=2 WHERE id=?1", [id])
        .unwrap();
    f.reader
        .conn
        .execute("DELETE FROM events WHERE id=?1", [id])
        .unwrap();
    for table in [
        "retention_activity_pending",
        "retention_activity_sources",
        "retention_activity_runs",
    ] {
        let count: i64 = f
            .reader
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
    assert_eq!(
        f.reader
            .conn
            .query_row(
                "SELECT revision FROM retention_activity_control WHERE id=1",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}
#[test]
fn row_budget_counts_all_rows_and_activity_headers_never_decode_bodies() {
    let f = Fixture::new();
    for at in 0..501 {
        f.insert(message("r"), at);
    }
    let report = f.tick();
    assert_eq!(report.inspected_rows, 500);
    assert_eq!(report.decode_bytes, 0);
    f.drain();
    assert_eq!(
        f.reader.run_activity("r").unwrap().last_actual_activity_at,
        Some(500)
    );
}
#[test]
fn exact_decode_boundary_deferral_and_oversized_rows_are_bounded() {
    let f = Fixture::new();
    f.drain();
    let g = goal("G", "thread", "root", ThreadGoalPhase::Working);
    let mut payload = serde_json::to_string(&snapshot(g)).unwrap();
    payload.push_str(&" ".repeat(ACTIVITY_DECODE_BYTES_PER_TICK - payload.len()));
    f.reader.conn.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Orchestrator','ThreadGoalUpdated','root',1,?1)",[&payload]).unwrap();
    let report = f.tick();
    assert_eq!(report.decode_bytes, ACTIVITY_DECODE_BYTES_PER_TICK);
    assert!(f.reader.activity_goal_protected("thread").unwrap());
    payload.push(' ');
    // The oversized row has a known family; the global unknown-root contract
    // is tested separately below. Other known families must still progress.
    f.reader.conn.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES('oversized','other-thread','oversized',0)", []).unwrap();
    f.reader.conn.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Orchestrator','ThreadGoalUpdated','oversized',1,?1)",[&payload]).unwrap();
    let report = f.tick();
    assert_eq!(report.oversized, 1);
    assert_eq!(report.decode_bytes, 0);
    assert!(!f.reader.run_activity("oversized").unwrap().ready);
    f.insert(message("healthy"), 100);
    f.drain();
    assert!(
        f.reader.run_activity("healthy").unwrap().ready,
        "a failed source must not starve a different candidate"
    );
}
#[test]
fn goal_transfer_complete_and_replacement_never_resurrect_an_old_binding() {
    let f = Fixture::new();
    let g1 = goal("G1", "A", "root1", ThreadGoalPhase::Working);
    f.insert(snapshot(g1.clone()), 10);
    let mut g2 = goal("G2", "A", "root2", ThreadGoalPhase::Working);
    f.insert(snapshot(g2.clone()), 20);
    f.drain();
    assert!(f.reader.activity_goal_protected("A").unwrap());
    g2.thread_id = "B".into();
    g2.related_root_run_ids.push("root2".into());
    g2.root_run_id = "root3".into();
    g2.phase = ThreadGoalPhase::Complete;
    f.insert(snapshot(g2.clone()), 30);
    f.drain();
    assert!(!f.reader.activity_goal_protected("A").unwrap());
    assert!(!f.reader.activity_goal_protected("B").unwrap());
    let mut cache = std::collections::BTreeMap::new();
    let mut report = ActivityReport::default();
    let GoalPreparation::Ready(decision) =
        prepare_goal(&f.reader.conn, &g1, 1, 0, true, &mut report, &mut cache).unwrap()
    else {
        panic!("small goal fits");
    };
    assert!(super::goal(&f.reader.conn, &g1, 1, "[]", &decision).unwrap());
    assert!(!f.reader.activity_goal_protected("A").unwrap());
    let mut stale = g2;
    stale.thread_id = "A".into();
    stale.root_run_id = "root2".into();
    stale.related_root_run_ids.clear();
    stale.phase = ThreadGoalPhase::Working;
    f.insert(snapshot(stale), 40);
    f.drain();
    assert!(!f.reader.activity_goal_protected("A").unwrap());
    assert!(!f.reader.activity_goal_protected("B").unwrap());
}
#[test]
fn receipt_status_and_goal_updates_are_not_activity() {
    let f = Fixture::new();
    f.reader.conn.execute("INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns) VALUES('p','r','r','text','accepted',10,10)",[]).unwrap();
    f.drain();
    f.reader.conn.execute("UPDATE host_input_receipts SET status='delivered',status_at_ns=100 WHERE prompt_id='p'",[]).unwrap();
    f.insert(
        snapshot(goal("G", "thread", "r", ThreadGoalPhase::Blocked)),
        1000,
    );
    f.drain();
    let state = f.reader.run_activity("r").unwrap();
    assert_eq!(state.last_actual_activity_at, Some(10));
    assert!(f.reader.activity_goal_protected("thread").unwrap());
}
#[test]
fn migration_boundary_captures_legacy_fallback_once_and_new_empty_runs_are_ready() {
    let f = Fixture::new();
    f.reader.conn.execute("INSERT INTO run_contexts(run_id,role,name,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) VALUES('old','worker','synthetic','{}','[]','done',0,100)",[]).unwrap();
    f.reader.conn.execute("UPDATE retention_activity_control SET legacy_context_rowid=(SELECT max(rowid) FROM run_contexts)",[]).unwrap();
    f.reader.conn.execute("UPDATE retention_activity_cursors SET high_water=(SELECT max(rowid) FROM run_contexts) WHERE kind='run_contexts'",[]).unwrap();
    f.drain();
    let first = f.reader.run_activity("old").unwrap();
    assert_eq!(first.last_actual_activity_at, None);
    assert_eq!(first.legacy_activity_upper_bound, Some(100));
    assert!(first.ready);
    f.reader
        .conn
        .execute(
            "UPDATE run_contexts SET updated_at_ns=1000 WHERE run_id='old'",
            [],
        )
        .unwrap();
    f.drain();
    assert_eq!(
        f.reader
            .run_activity("old")
            .unwrap()
            .legacy_activity_upper_bound,
        Some(100)
    );
    f.reader.conn.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES('new','new-thread','new',0)",[]).unwrap();
    f.drain();
    assert!(f.reader.run_activity("new").unwrap().ready);
}

#[test]
fn failed_writer_ack_keeps_dirty_sources_across_restart_and_retry() {
    let mut f = Fixture::new();
    f.drain();
    f.insert(message("run"), 100);
    f.reader.conn.execute_batch("CREATE TRIGGER reject_activity BEFORE INSERT ON retention_activity_runs BEGIN SELECT RAISE(ABORT,'injected projection failure'); END").unwrap();
    assert!(
        f.storage
            .handle()
            .apply_activity_page(f.reader.activity_page().unwrap())
            .is_err()
    );
    f.reader = Database::open(&f.config).unwrap();
    assert!(!f.reader.run_activity("run").unwrap().ready);
    assert!(f.reader.activity_report().unwrap().pending_sources > 0);
    f.reader
        .conn
        .execute_batch("DROP TRIGGER reject_activity")
        .unwrap();
    f.drain();
    let activity = f.reader.run_activity("run").unwrap();
    assert!(activity.ready);
    assert_eq!(activity.last_actual_activity_at, Some(100));
}

#[test]
fn compressed_sources_charge_expansion_and_preflight_without_reading_oversized_bodies() {
    let f = Fixture::new();
    f.drain();
    let mut payload = serde_json::to_vec(&snapshot(goal(
        "compressed",
        "thread",
        "root",
        ThreadGoalPhase::Working,
    )))
    .unwrap();
    payload.resize(ACTIVITY_DECODE_BYTES_PER_TICK, b' ');
    let compressed = zstd::bulk::compress(&payload, 1).unwrap();
    assert!(compressed.len() + 18 < payload.len());
    f.reader.conn.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Orchestrator','ThreadGoalUpdated','root',1,?1)",[compressed]).unwrap();
    let report = f.tick();
    assert_eq!(report.decode_bytes, ACTIVITY_DECODE_BYTES_PER_TICK);
    assert!(f.reader.activity_goal_protected("thread").unwrap());
    payload.push(b' ');
    let compressed = zstd::bulk::compress(&payload, 1).unwrap();
    f.reader.conn.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Orchestrator','ThreadGoalUpdated','too-large',1,?1)",[compressed]).unwrap();
    let report = f.tick();
    assert_eq!(report.oversized, 1);
    assert_eq!(
        report.decode_bytes, 18,
        "only the bounded content-size preflight is read"
    );
    assert!(!f.reader.run_activity("too-large").unwrap().ready);
}
#[test]
fn bound_run_enumeration_is_keyset_and_binding_changes_have_a_separate_revision() {
    let f = Fixture::new();
    let before = f.reader.activity_binding_revision().unwrap();
    for run in ["run-1", "run-2", "run-3"] {
        f.reader.conn.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,'thread','run-1',1)",[run]).unwrap();
    }
    let changed = f.reader.activity_binding_revision().unwrap();
    assert!(changed > before);
    let first = f.reader.activity_thread_run_page("thread", 0, 2).unwrap();
    assert_eq!(first.len(), 2);
    let rest = f
        .reader
        .activity_thread_run_page("thread", first[1].0, 2)
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].1, "run-3");
    f.insert(message("run-1"), 100);
    assert_eq!(f.reader.activity_binding_revision().unwrap(), changed);
    f.reader
        .conn
        .execute(
            "UPDATE thread_runs SET thread_id='other' WHERE run_id='run-3'",
            [],
        )
        .unwrap();
    assert!(f.reader.activity_binding_revision().unwrap() > changed);
}

#[test]
fn corrupt_compressed_source_does_not_starve_healthy_runs() {
    let f = Fixture::new();
    f.drain();
    let mut payload = zstd::bulk::compress(b"{invalid json with a valid zstd header}", 1).unwrap();
    payload.truncate(payload.len() - 3);
    f.reader.conn.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Lifecycle','AgentRunStateChanged','broken',1,?1)",[payload]).unwrap();
    f.insert(message("healthy"), 100);
    f.drain();
    assert!(!f.reader.run_activity("broken").unwrap().ready);
    assert!(f.reader.run_activity("healthy").unwrap().ready);
    assert_eq!(
        f.reader
            .run_activity("healthy")
            .unwrap()
            .last_actual_activity_at,
        Some(100)
    );
}
#[test]
fn known_activity_does_not_create_a_legacy_fallback_from_generic_save_time() {
    let f = Fixture::new();
    f.reader.conn.execute("INSERT INTO run_contexts(run_id,role,name,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) VALUES('old','worker','synthetic','{}','[]','done',0,1000)",[]).unwrap();
    f.reader.conn.execute("UPDATE retention_activity_control SET legacy_context_rowid=(SELECT max(rowid) FROM run_contexts)",[]).unwrap();
    f.reader.conn.execute("UPDATE retention_activity_cursors SET high_water=(SELECT max(rowid) FROM run_contexts) WHERE kind='run_contexts'",[]).unwrap();
    f.insert(message("old"), 100);
    f.drain();
    let a = f.reader.run_activity("old").unwrap();
    assert!(a.ready);
    assert_eq!(a.last_actual_activity_at, Some(100));
    assert_eq!(a.legacy_activity_upper_bound, None);
}

#[test]
fn initial_high_water_finishes_while_new_events_continue_to_arrive() {
    let f = Fixture::new();
    for n in 0..600 {
        f.insert(message("old"), n);
    }
    f.reader
        .conn
        .execute(
            "UPDATE retention_activity_cursors SET high_water=600 WHERE stream<21",
            [],
        )
        .unwrap();
    let mut completed = false;
    for n in 0..8 {
        for k in 0..100 {
            f.insert(message("new"), 1000 + n * 100 + k);
        }
        let report = f.tick();
        assert!(report.inspected_rows <= 500);
        assert!(
            report
                .cursors
                .iter()
                .filter(|c| c.stream < 21)
                .all(|c| c.high_water == 600)
        );
        if !report.backfill_pending {
            completed = true;
            break;
        }
    }
    assert!(
        completed,
        "new writes must not move the frozen initial high-water mark"
    );
    f.drain();
    assert!(f.reader.run_activity("new").unwrap().ready);
}

#[test]
fn prior_goal_lineage_is_budgeted_at_the_exact_boundary_and_scopes_oversize_failure() {
    for excess in [0, 1] {
        let f = Fixture::new();
        f.drain();
        f.insert(
            snapshot(goal("g", "a", "old", ThreadGoalPhase::Complete)),
            1,
        );
        f.drain();
        let event = snapshot(goal("g", "b", "new", ThreadGoalPhase::Working));
        let source_bytes = serde_json::to_vec(&event).unwrap().len();
        let prefix = serde_json::to_string(&vec!["new"]).unwrap();
        let lineage_bytes = ACTIVITY_DECODE_BYTES_PER_TICK - source_bytes + excess;
        let related = prefix.clone() + &" ".repeat(lineage_bytes - prefix.len());
        f.reader
            .conn
            .execute(
                "UPDATE retention_activity_goals SET related=?1 WHERE goal_id='g'",
                [related],
            )
            .unwrap();
        f.insert(event, 2);
        f.insert(message("healthy"), 3);
        let report = f.tick();
        assert_eq!(report.oversized, excess);
        assert!(!report.cas_rejected);
        assert_eq!(
            report.decode_bytes,
            if excess == 0 {
                ACTIVITY_DECODE_BYTES_PER_TICK
            } else {
                source_bytes
            }
        );
        assert_eq!(f.reader.activity_goal_protected("b").unwrap(), excess != 0);
        assert_eq!(f.reader.run_activity("new").unwrap().ready, excess == 0);
        // The bad goal does not consume another candidate's progress or create
        // global uncertainty. Header-only message activity costs no decode bytes.
        let healthy = f.reader.run_activity("healthy").unwrap();
        assert!(healthy.ready);
        assert_eq!(healthy.last_actual_activity_at, Some(3));
        let thread: String = f
            .reader
            .conn
            .query_row(
                "SELECT thread_id FROM retention_activity_goals WHERE goal_id='g'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(thread, "a");
    }
}

#[test]
fn repeated_goal_snapshots_reuse_reader_lineage_and_defer_before_another_payload() {
    let f = Fixture::new();
    f.drain();
    f.insert(
        snapshot(goal("g", "a", "old", ThreadGoalPhase::Complete)),
        1,
    );
    f.drain();
    let prefix = serde_json::to_string(&vec!["new"]).unwrap();
    let lineage_bytes = ACTIVITY_DECODE_BYTES_PER_TICK / 2;
    f.reader
        .conn
        .execute(
            "UPDATE retention_activity_goals SET related=?1 WHERE goal_id='g'",
            [prefix.clone() + &" ".repeat(lineage_bytes - prefix.len())],
        )
        .unwrap();
    let mut g = goal("g", "b", "new", ThreadGoalPhase::Working);
    g.objective = "x".repeat(2_000);
    let event = snapshot(g);
    let source_bytes = serde_json::to_vec(&event).unwrap().len();
    for _ in 0..ACTIVITY_ROWS_PER_TICK {
        f.insert(event.clone(), 2);
    }
    let report = f.tick();
    assert_eq!(report.oversized, 0);
    assert!(report.inspected_rows > 1 && report.inspected_rows < ACTIVITY_ROWS_PER_TICK);
    assert_eq!(
        report.decode_bytes,
        lineage_bytes + (report.inspected_rows - 1) * source_bytes
    );
    assert!(report.pending_sources > 0);
    f.drain();
    assert!(!f.reader.activity_goal_protected("b").unwrap());
}

#[test]
fn another_projection_commit_rejects_a_stale_reader_goal_decision() {
    let f = Fixture::new();
    f.drain();
    f.insert(snapshot(goal("g", "a", "old", ThreadGoalPhase::Working)), 1);
    f.drain();
    let mut transferred = goal("g", "b", "new", ThreadGoalPhase::Complete);
    transferred.related_root_run_ids = vec!["old".into()];
    f.insert(snapshot(transferred), 2);
    let first = f.reader.activity_page().unwrap();
    let stale = f.reader.activity_page().unwrap();
    assert!(
        !f.storage
            .handle()
            .apply_activity_page(first)
            .unwrap()
            .cas_rejected
    );
    assert!(
        f.storage
            .handle()
            .apply_activity_page(stale)
            .unwrap()
            .cas_rejected
    );
    assert!(!f.reader.activity_goal_protected("a").unwrap());
    assert!(!f.reader.activity_goal_protected("b").unwrap());
    let current: String = f
        .reader
        .conn
        .query_row(
            "SELECT thread_id FROM retention_activity_goals WHERE goal_id='g'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(current, "b");
}

#[test]
fn unreadable_goal_with_unknown_root_holds_globally_and_reports_bounded_ids_until_removed() {
    let f = Fixture::new();
    f.insert(
        snapshot(goal(
            "g",
            "old-thread",
            "old-root",
            ThreadGoalPhase::Complete,
        )),
        1,
    );
    f.insert(message("old-root"), 1);
    f.drain();
    let mut g = goal("g", "old-thread", "unbound-root", ThreadGoalPhase::Working);
    g.objective = "x".repeat(ACTIVITY_DECODE_BYTES_PER_TICK + 1);
    let id = f.insert(snapshot(g), 2);
    f.drain();
    let report = f.reader.activity_report().unwrap();
    assert_eq!(report.global_failures, 1);
    assert_eq!(report.global_failure_event_ids, [id]);
    assert_eq!(
        f.reader.activity_thread_hold("old-thread").unwrap(),
        Some("SourceRevisionBehind")
    );
    assert!(!f.reader.run_activity("old-root").unwrap().ready);
    f.reader
        .conn
        .execute("DELETE FROM events WHERE id=?1", [id])
        .unwrap();
    f.drain();
    assert_eq!(f.reader.activity_report().unwrap().global_failures, 0);
    assert_eq!(f.reader.activity_thread_hold("old-thread").unwrap(), None);
    assert!(f.reader.run_activity("old-root").unwrap().ready);
}

#[test]
fn unreadable_goal_with_unique_header_binding_holds_only_its_run_family() {
    let f = Fixture::new();
    f.reader.conn.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES('bound-root','source-thread','bound-root',0)", []).unwrap();
    f.insert(message("bound-root"), 1);
    f.insert(message("unrelated-root"), 1);
    let mut g = goal("g", "child-thread", "bound-root", ThreadGoalPhase::Working);
    g.objective = "x".repeat(ACTIVITY_DECODE_BYTES_PER_TICK + 1);
    f.insert(snapshot(g), 2);
    f.drain();
    assert_eq!(f.reader.activity_report().unwrap().global_failures, 0);
    assert!(!f.reader.run_activity("bound-root").unwrap().ready);
    assert!(f.reader.run_activity("unrelated-root").unwrap().ready);
    assert_eq!(
        f.reader.activity_thread_hold("unrelated-thread").unwrap(),
        None
    );
}

#[test]
fn unattributed_committed_message_holds_empty_families_before_and_after_projection() {
    let f = Fixture::new();
    f.drain();
    f.reader.conn.execute_batch("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,wall_clock_ns,payload) VALUES(1,0,'Message','MessageCompleted',1,'{}')").unwrap();
    assert_eq!(
        f.reader.activity_thread_hold("unrelated-empty").unwrap(),
        Some("SourceRevisionBehind")
    );
    f.drain();
    assert_eq!(
        f.reader.activity_thread_hold("unrelated-empty").unwrap(),
        Some("SourceRevisionBehind")
    );
    assert_eq!(f.reader.activity_report().unwrap().global_failures, 1);
}

#[test]
fn deleted_goal_source_updates_only_its_projection_through_an_indexed_lookup() {
    let f = Fixture::new();
    let removed = f.insert(
        snapshot(goal(
            "one",
            "thread-one",
            "run-one",
            ThreadGoalPhase::Complete,
        )),
        100,
    );
    f.insert(
        snapshot(goal(
            "two",
            "thread-two",
            "run-two",
            ThreadGoalPhase::Complete,
        )),
        200,
    );
    f.drain();
    let plan = f
        .reader
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {INVALIDATE_GOAL_SOURCE_SQL}"))
        .unwrap()
        .query_map([removed], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.starts_with("SEARCH retention_activity_goals ")
                && line.contains("retention_activity_goals_event")),
        "{plan:?}"
    );
    assert!(
        plan.iter().all(|line| !line.starts_with("SCAN ")),
        "{plan:?}"
    );
    f.reader
        .conn
        .execute("DELETE FROM events WHERE id=?1", [removed])
        .unwrap();
    f.drain();
    let states = f
        .reader
        .conn
        .prepare("SELECT goal_id,uncertain FROM retention_activity_goals ORDER BY goal_id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(states, vec![("one".into(), true), ("two".into(), false)]);
}
