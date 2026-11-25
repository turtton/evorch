//! The decision reads canonical rows only: no hook, projection or clock is involved, so
//! each case writes the rows a path would write and decides again.
use super::*;
use crate::{Database, StorageConfig};
use event_bus::{MessageEvent, ThreadGoalUsage};
use rusqlite::params;
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

const CUTOFF: i64 = 1_000;

struct Fixture {
    _dir: tempfile::TempDir,
    db: Database,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("decision.db"),
            ..Default::default()
        };
        let db = Database::open(&config).unwrap();
        Self { _dir: dir, db }
    }
    fn sql(&self, text: &str, p: impl rusqlite::Params) {
        self.db.conn.execute(text, p).unwrap();
    }
    fn event(&self, kind: EventKind, at: i64) -> i64 {
        self.sql(
            "INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,?1,?2,?3,?4,?5)",
            params![
                crate::repo::event::kind_name(&kind),
                kind.subkind_name(),
                kind.primary_run_id(),
                at,
                serde_json::to_string(&kind).unwrap()
            ],
        );
        self.db.conn.last_insert_rowid()
    }
    fn raw(&self, kind: &str, sub: &str, run: &str, at: i64, payload: &str) -> i64 {
        self.sql(
            "INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,?1,?2,?3,?4,?5)",
            params![kind, sub, run, at, payload],
        );
        self.db.conn.last_insert_rowid()
    }
    fn decide(&self, family: &[FamilyMember]) -> Verdict {
        self.decide_with(family, &DecisionBudget::default()).verdict
    }
    fn decide_with(&self, family: &[FamilyMember], budget: &DecisionBudget) -> Decision {
        self.db
            .decide_family_archive(family, CUTOFF, budget)
            .unwrap()
    }
    fn steps(&self, family: &[FamilyMember]) -> usize {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let _ = self.db.conn.progress_handler(
            1,
            Some(move || {
                seen.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        self.decide(family);
        let _ = self.db.conn.progress_handler(1, None::<fn() -> bool>);
        count.load(Ordering::Relaxed)
    }
}

fn member(thread: &str, runs: &[&str]) -> FamilyMember {
    FamilyMember {
        thread_id: thread.into(),
        created_ns: 0,
        runs: runs.iter().map(|r| (*r).into()).collect(),
    }
}
fn one() -> Vec<FamilyMember> {
    vec![member("t1", &["r1"])]
}
fn due() -> Verdict {
    Verdict::Due
}
fn held(h: Hold) -> Verdict {
    Verdict::Hold(h)
}
fn message(run: &str) -> EventKind {
    MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: "text".into(),
    }
    .into()
}
fn state(run: &str, from: AgentRunPhase, to: AgentRunPhase) -> EventKind {
    LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from,
        to,
        reason: None,
        waiting: None,
    }
    .into()
}
fn goal(id: &str, thread: &str, root: &str, phase: ThreadGoalPhase) -> EventKind {
    OrchestratorEvent::ThreadGoalUpdated {
        snapshot: ThreadGoalSnapshot {
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
        },
    }
    .into()
}

#[test]
fn the_migration_adds_indexes_and_no_derived_state() {
    let f = Fixture::new();
    let names: Vec<String> = f
        .db
        .conn
        .prepare("SELECT name FROM sqlite_master WHERE name LIKE 'retention_activity%' OR name LIKE 'retention_decision%' OR name LIKE 'fence_retention_activity%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(names.is_empty(), "{names:?}");
    for index in [
        "idx_events_goal_by_run",
        "host_input_receipts_run_accepted",
        "host_chat_submissions_run_accepted",
        "idx_retention_questions_root_pending",
    ] {
        let found: bool =
            f.db.conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)",
                    [index],
                    |r| r.get(0),
                )
                .unwrap();
        assert!(found, "{index}");
    }
}

fn plan(f: &Fixture, text: &str) -> Vec<String> {
    let mut stmt =
        f.db.conn
            .prepare(&format!("EXPLAIN QUERY PLAN {text}"))
            .unwrap();
    let nulls = vec![rusqlite::types::Value::Null; stmt.parameter_count()];
    let rows = stmt
        .query_map(rusqlite::params_from_iter(nulls), |r| r.get::<_, String>(3))
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

#[test]
fn every_statement_pins_an_index_and_never_scans_or_sorts_a_source_table() {
    let f = Fixture::new();
    let statements = [
        sql::RECENT_RUNS,
        sql::RUN_TAIL,
        sql::GOAL_LATEST,
        sql::EVENT_SHAPE,
        sql::EVENT_BODY,
        sql::RECENT_RECEIPTS,
        sql::LEGACY_FALLBACK,
        sql::HANDOFF_SHAPE,
        sql::HANDOFF_BODY,
        sql::QUESTIONS,
        sql::EVAL_SAMPLES,
        sql::LANES,
        sql::THREAD_RUNS,
    ];
    for text in statements {
        let lines = plan(&f, text);
        for line in &lines {
            let scans_a_table = line.starts_with("SCAN ") && !line.starts_with("SCAN j VIRTUAL");
            assert!(!scans_a_table, "{text}\n{lines:?}");
            assert!(!line.contains("TEMP B-TREE"), "{text}\n{lines:?}");
        }
    }
    for (text, index) in [
        (sql::RECENT_RUNS, "idx_events_run_id"),
        (sql::RUN_TAIL, "idx_events_run_id"),
        (sql::GOAL_LATEST, "idx_events_goal_by_run"),
        (sql::RECENT_RECEIPTS, "host_input_receipts_run_accepted"),
        (sql::RECENT_RECEIPTS, "host_chat_submissions_run_accepted"),
        (sql::LEGACY_FALLBACK, "idx_events_run_id"),
    ] {
        let joined = plan(&f, text).join("\n");
        assert!(joined.contains(index), "{text}\n{joined}");
    }
}

#[test]
fn work_does_not_grow_when_unrelated_events_receipts_and_goals_are_added() {
    let f = Fixture::new();
    f.event(message("r1"), 10);
    f.event(goal("g", "t1", "r1", ThreadGoalPhase::Complete), 20);
    f.decide(&one());
    let before = f.steps(&one());
    for i in 0..500 {
        let run = format!("other-{}", i % 7);
        f.event(message(&run), 2_000 + i);
        f.event(
            goal(&format!("g{i}"), "t-other", &run, ThreadGoalPhase::Working),
            2_000 + i,
        );
        f.sql(
            "INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns) VALUES(?1,?2,?2,'t','delivered',5,5)",
            params![format!("p{i}"), run],
        );
    }
    assert_eq!(before, f.steps(&one()));
}

#[test]
fn recent_activity_holds_and_the_boundary_is_inclusive() {
    let f = Fixture::new();
    let family = one();
    assert_eq!(f.decide(&family), due());
    for (name, event, at, expected) in [
        ("old message", message("r1"), CUTOFF - 1, due()),
        ("message at cutoff", message("r1"), CUTOFF, due()),
        (
            "message after cutoff",
            message("r1"),
            CUTOFF + 1,
            held(Hold::RecentActivity),
        ),
        (
            "turn completed",
            LifecycleEvent::TurnCompleted {
                run_id: "r1".into(),
                context_len: 1,
            }
            .into(),
            CUTOFF + 1,
            held(Hold::RecentActivity),
        ),
        (
            "became running",
            state("r1", AgentRunPhase::Pending, AgentRunPhase::Running),
            CUTOFF + 1,
            held(Hold::RecentActivity),
        ),
        (
            "left running",
            state("r1", AgentRunPhase::Running, AgentRunPhase::Waiting),
            CUTOFF + 1,
            held(Hold::RecentActivity),
        ),
        (
            "waiting transition is not activity",
            state("r1", AgentRunPhase::Pending, AgentRunPhase::Waiting),
            CUTOFF + 1,
            due(),
        ),
    ] {
        let id = f.event(event, at);
        assert_eq!(f.decide(&family), expected, "{name}");
        f.sql("DELETE FROM events WHERE id=?1", [id]);
    }
}

#[test]
fn commits_by_any_path_are_seen_by_the_next_decision() {
    let f = Fixture::new();
    let family = one();
    assert_eq!(f.decide(&family), due());
    // raw SQL, as a second connection, a copied handoff row or a rewrite would write it
    let id = f.raw("Message", "MessageCompleted", "elsewhere", CUTOFF + 5, "{}");
    assert_eq!(f.decide(&family), due());
    f.sql("UPDATE events SET run_id='r1' WHERE id=?1", [id]);
    assert_eq!(f.decide(&family), held(Hold::RecentActivity));
    f.sql(
        "UPDATE events SET wall_clock_ns=?2 WHERE id=?1",
        [id, CUTOFF],
    );
    assert_eq!(f.decide(&family), due());
    f.sql(
        "UPDATE events SET wall_clock_ns=?2 WHERE id=?1",
        [id, CUTOFF + 1],
    );
    assert_eq!(f.decide(&family), held(Hold::RecentActivity));
    f.sql("DELETE FROM events WHERE id=?1", [id]);
    assert_eq!(f.decide(&family), due());
    f.sql(
        "INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns) VALUES('p','r1','r1','t','delivered',?1,?1)",
        [CUTOFF],
    );
    assert_eq!(
        f.decide(&family),
        due(),
        "a copied receipt keeps its own time"
    );
    f.sql(
        "UPDATE host_input_receipts SET accepted_at_ns=?1",
        [CUTOFF + 1],
    );
    assert_eq!(f.decide(&family), held(Hold::RecentInput));
    f.sql("DELETE FROM host_input_receipts", []);
    f.sql(
        "INSERT INTO host_chat_submissions(submission_id,fingerprint,run_id,provenance_json,accepted_at_ns) VALUES('s','f','r1','{}',?1)",
        [CUTOFF + 1],
    );
    assert_eq!(f.decide(&family), held(Hold::RecentInput));
}

#[test]
fn only_a_conversation_with_no_other_evidence_falls_back_to_its_save_time() {
    let f = Fixture::new();
    let family = one();
    let run = |count: i64, saved: i64| {
        f.sql("DELETE FROM run_contexts", []);
        f.sql(
            "INSERT INTO run_contexts(run_id,role,name,parent_run_id,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns,message_count) VALUES('r1','worker','w',NULL,'{}','[]','done',1,?1,?2)",
            [saved, count],
        );
    };
    run(3, CUTOFF + 1);
    assert_eq!(f.decide(&family), held(Hold::LegacyActivityFallback));
    run(3, CUTOFF);
    assert_eq!(f.decide(&family), due(), "an old save never delays");
    run(0, CUTOFF + 1);
    assert_eq!(
        f.decide(&family),
        due(),
        "a run without a conversation is not delayed"
    );
    run(3, CUTOFF + 1);
    let id = f.event(message("r1"), CUTOFF - 1);
    assert_eq!(
        f.decide(&family),
        due(),
        "an event is the evidence; the save is ignored"
    );
    f.sql("DELETE FROM events WHERE id=?1", [id]);
    f.sql(
        "INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns) VALUES('p','r1','r1','t','delivered',?1,?1)",
        [CUTOFF - 1],
    );
    assert_eq!(
        f.decide(&family),
        due(),
        "an old receipt is the evidence too"
    );
}

#[test]
fn a_thread_without_runs_is_judged_by_its_creation_time_alone() {
    let f = Fixture::new();
    let mut empty = member("t1", &[]);
    empty.created_ns = CUTOFF;
    assert_eq!(f.decide(std::slice::from_ref(&empty)), due());
    empty.created_ns = CUTOFF + 1;
    assert_eq!(f.decide(std::slice::from_ref(&empty)), held(Hold::NotDue));
    let mut family = one();
    family.push(empty);
    assert_eq!(
        f.decide(&family),
        held(Hold::NotDue),
        "the newest member rules the family"
    );
}

#[test]
fn a_family_is_held_by_one_active_member_run_bound_only_in_the_database() {
    let f = Fixture::new();
    let family = vec![member("t1", &["r1"]), member("t2", &["r2"])];
    f.sql(
        "INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES('late','t2','r2',1)",
        [],
    );
    assert_eq!(f.decide(&family), due());
    f.event(message("late"), CUTOFF + 1);
    assert_eq!(f.decide(&family), held(Hold::RecentActivity));
    assert_eq!(
        f.db.family_runs(&family, &DecisionBudget::default())
            .unwrap(),
        Ok(vec!["late".to_owned(), "r1".into(), "r2".into()])
    );
}

#[test]
fn goal_protection_follows_the_newest_snapshot_and_the_stored_payload() {
    let f = Fixture::new();
    let family = one();
    f.event(goal("g1", "t1", "r1", ThreadGoalPhase::Working), 10);
    assert_eq!(f.decide(&family), held(Hold::UnfinishedGoal));
    let complete = f.event(goal("g1", "t1", "r1", ThreadGoalPhase::Complete), 20);
    assert_eq!(f.decide(&family), due());
    // a payload rewritten outside any hook is read again at the next decision
    let working = serde_json::to_string(&goal("g1", "t1", "r1", ThreadGoalPhase::Working)).unwrap();
    f.sql(
        "UPDATE events SET payload=?2 WHERE id=?1",
        params![complete, working],
    );
    assert_eq!(f.decide(&family), held(Hold::UnfinishedGoal));
    for phase in [ThreadGoalPhase::Blocked, ThreadGoalPhase::Checking] {
        f.event(goal("g1", "t1", "r1", phase), 30);
        assert_eq!(f.decide(&family), held(Hold::UnfinishedGoal), "{phase:?}");
    }
    f.event(goal("g2", "t1", "r1", ThreadGoalPhase::Complete), 40);
    assert_eq!(f.decide(&family), due(), "a replaced goal never comes back");
}

#[test]
fn a_goal_handed_to_another_family_no_longer_protects_its_source() {
    let f = Fixture::new();
    let family = one();
    f.event(goal("g1", "t1", "r1", ThreadGoalPhase::Working), 10);
    let receipt = |state: &str, goal_id: &str| {
        f.sql("DELETE FROM lane_handoffs", []);
        f.sql(
            "INSERT INTO lane_handoffs(source_run_id,source_incarnation,target_run_id,lane_id,request_json,state,goal_json) VALUES('r1','i','target','lane','{}',?1,?2)",
            params![state, format!("{{\"goal_id\":\"{goal_id}\"}}")],
        );
    };
    receipt("prepared", "g1");
    assert_eq!(
        f.decide(&family),
        held(Hold::UnfinishedGoal),
        "not committed yet"
    );
    receipt("committed", "g2");
    assert_eq!(
        f.decide(&family),
        held(Hold::UnfinishedGoal),
        "another goal's receipt"
    );
    for state in ["committed", "started", "recoverable_start_failure"] {
        receipt(state, "g1");
        assert_eq!(f.decide(&family), due(), "{state}");
    }
    f.event(goal("g3", "t1", "r1", ThreadGoalPhase::Working), 50);
    assert_eq!(
        f.decide(&family),
        held(Hold::UnfinishedGoal),
        "a goal set after the handoff protects"
    );
}

#[test]
fn a_snapshot_naming_a_thread_outside_the_family_or_unreadable_holds() {
    let f = Fixture::new();
    let family = one();
    let id = f.event(goal("g1", "elsewhere", "r1", ThreadGoalPhase::Complete), 10);
    assert_eq!(f.decide(&family), held(Hold::GoalOutsideFamily));
    f.sql("UPDATE events SET payload='not json' WHERE id=?1", [id]);
    assert_eq!(f.decide(&family), held(Hold::GoalUndecodable));
}

#[test]
fn every_budget_turns_an_unbounded_read_into_a_hold() {
    let f = Fixture::new();
    let family = vec![member("t1", &["r1", "r2", "r3"])];
    let tight = DecisionBudget {
        family_entries: 3,
        ..DecisionBudget::default()
    };
    assert_eq!(
        f.decide_with(&family, &tight).verdict,
        held(Hold::FamilyRunBudget)
    );
    for _ in 0..3 {
        f.raw("Tool", "ToolCompleted", "r1", CUTOFF + 1, "{}");
    }
    let per_run = DecisionBudget {
        tail_rows_per_run: 2,
        ..DecisionBudget::default()
    };
    let d = f.decide_with(&family, &per_run);
    assert_eq!(d.verdict, held(Hold::ActivityProbeBudget));
    assert!(d.stats.tail_rows <= 3);
    let total = DecisionBudget {
        tail_rows: 2,
        ..DecisionBudget::default()
    };
    assert_eq!(
        f.decide_with(&family, &total).verdict,
        held(Hold::ActivityProbeBudget)
    );
    f.sql("DELETE FROM events", []);
    for (i, run) in ["r1", "r2", "r3"].iter().enumerate() {
        f.event(
            goal(&format!("g{i}"), "t1", run, ThreadGoalPhase::Complete),
            10,
        );
    }
    let few = DecisionBudget {
        goal_decodes: 2,
        ..DecisionBudget::default()
    };
    assert_eq!(
        f.decide_with(&family, &few).verdict,
        held(Hold::GoalDecodeBudget)
    );
    let bytes = DecisionBudget {
        decode_bytes: 100,
        ..DecisionBudget::default()
    };
    assert_eq!(
        f.decide_with(&family, &bytes).verdict,
        held(Hold::GoalDecodeBudget)
    );
    let default = f.decide_with(&family, &DecisionBudget::default());
    assert_eq!(default.verdict, due());
    assert_eq!(default.stats.goal_decodes, 3);
    assert!(default.stats.decode_bytes <= DECODE_BYTES_PER_DECISION);
}

#[test]
fn unbackfilled_headers_hold_every_family() {
    let f = Fixture::new();
    f.sql(
        "INSERT INTO events(schema_version,monotonic_ns,kind,run_id,wall_clock_ns,payload) VALUES(1,0,'Message','unrelated',1,'{}')",
        [],
    );
    assert_eq!(f.decide(&one()), held(Hold::HeaderBackfillPending));
}

#[test]
fn protections_are_read_from_their_own_tables() {
    let f = Fixture::new();
    let family = one();
    f.sql("INSERT INTO thread_start_states(thread_id,root_run_id,project_root,repo_prefix,head_commit,diff,captured_at_ns,replayable) VALUES('t1','r1','/fixture','','fixture',X'',0,1)", []);
    f.sql("INSERT INTO eval_samples(thread_id,problem,focus_roles,created_at_ns,updated_at_ns) VALUES('t1','p','[]',0,0)", []);
    assert_eq!(f.decide(&family), held(Hold::EvaluationSample));
    f.sql("DELETE FROM eval_samples", []);
    for (case, run_col, root, link) in [
        ("own", "r1", "x", None),
        ("root", "x", "r1", None),
        ("inherited", "x", "y", Some("r1")),
    ] {
        f.sql("DELETE FROM user_question_links", []);
        f.sql("DELETE FROM user_questions", []);
        f.sql("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES('q',?1,?2,'r','{}')", [run_col, root]);
        if let Some(run) = link {
            f.sql(
                "INSERT INTO user_question_links(question_id,run_id) VALUES('q',?1)",
                [run],
            );
        }
        assert_eq!(f.decide(&family), held(Hold::UnansweredQuestion), "{case}");
        f.sql("UPDATE user_questions SET answered=1", []);
        assert_eq!(f.decide(&family), due(), "{case} answered");
    }
    let lane = |policy: &str, closed: bool, revision: i64, projected: i64| {
        f.sql("DELETE FROM lane_closes", []);
        f.sql("DELETE FROM lanes", []);
        let brief = serde_json::json!({"objective":"o","inputs":[],"deliverables":[],"acceptance_criteria":[],"constraints":[],"report_policy":policy}).to_string();
        let lead = (policy != "detached").then_some("lead");
        f.sql("INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,request_hash,scope,lead_thread_id,policy,management_revision,management_projection_revision,brief_json,brief_revision,start_seed,root_run_id,generation,created_at_ns,updated_at_ns,last_activity_at_ns) VALUES('lane','t1','lead','r0','c','h','lane',?1,?2,?3,?4,?5,1,'{}','r1',1,0,0,0)", params![lead, policy, revision, projected, brief]);
        if closed {
            f.sql("INSERT INTO lane_closes(close_id,lane_id,kind,payload) VALUES('close','lane','human_closed','{}')", []);
            f.sql("UPDATE lanes SET current_close_id='close'", []);
        }
    };
    lane("attached", false, 1, 0);
    assert_eq!(f.decide(&family), held(Hold::OpenManagedLane));
    lane("summary", false, 1, 0);
    assert_eq!(f.decide(&family), held(Hold::OpenManagedLane));
    lane("attached", true, 1, 0);
    assert_eq!(f.decide(&family), due());
    lane("detached", false, 1, 0);
    assert_eq!(f.decide(&family), due());
    lane("detached", false, 3, 1);
    assert_eq!(
        f.decide(&family),
        held(Hold::FamilyUnresolved),
        "unapplied membership projection"
    );
}
