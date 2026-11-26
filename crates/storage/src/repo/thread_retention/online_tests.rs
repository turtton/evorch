use super::*;
use crate::thread_retention::{BindingInput, RetentionInput, SidebarThread, ThreadRetentionPolicy};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Fence {
    seals: Arc<AtomicUsize>,
    published: Arc<AtomicUsize>,
}
struct Token {
    seals: Arc<AtomicUsize>,
    published: Arc<AtomicUsize>,
}
impl CommitFence for Fence {
    fn try_seal(&self) -> Option<Box<dyn crate::thread_retention::online::SealedCommit>> {
        self.seals.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(Token {
            seals: self.seals.clone(),
            published: self.published.clone(),
        }))
    }
}
impl crate::thread_retention::online::SealedCommit for Token {
    fn publish(&mut self, _: &ConfirmedFamily) {
        self.published.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Token {
    fn drop(&mut self) {
        self.seals.fetch_sub(1, Ordering::SeqCst);
    }
}
fn fixture() -> (Database, OnlineRequest) {
    let db = Database::open_in_memory().unwrap();
    db.conn.execute_batch("INSERT INTO retention_binding VALUES(1,'/synthetic/root','/synthetic/sidebar',0);
      INSERT INTO threads(thread_id,root_run_id,archived,created_at_ns,updated_at_ns,source) VALUES('a','ra',1,0,0,'host');
      INSERT INTO thread_runs VALUES('ra','a','ra',0);
      INSERT INTO run_contexts(run_id,role,name,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) VALUES('ra','worker','w','{}','[]','done',1,0);").unwrap();
    let request = OnlineRequest {
        db_identity: db.retention_identity().unwrap(),
        seed: "a".into(),
        expected: OnlineClosure {
            threads: vec!["a".into()],
            runs: vec!["ra".into()],
        },
        binding: BindingInput {
            ownership_root: "/synthetic/root".into(),
            sidebar_path: "/synthetic/sidebar".into(),
        },
        policy: ThreadRetentionPolicy::default(),
        input: RetentionInput {
            threads: vec![SidebarThread {
                thread_id: "a".into(),
                archived: true,
                archived_at_s: Some(0),
                pinned: false,
                updated_at_s: 0,
                created_at_s: 0,
                archive_revision: Some(1),
                pin_revision: Some(0),
            }],
            ownership_root: "/synthetic/root".into(),
            sidebar_path: "/synthetic/sidebar".into(),
            now_ns: 7 * NANOS_PER_DAY,
        },
    };
    (db, request)
}
fn assert_hold(db: &Database, req: &OnlineRequest, budget: OnlineBudget, reason: &'static str) {
    let fence = Fence::default();
    assert_eq!(
        confirm(&db.conn, req, budget, &fence).unwrap(),
        OnlineOutcome::Hold(reason)
    );
    assert!(db.tombstones().unwrap().is_empty());
    assert_eq!(fence.published.load(Ordering::SeqCst), 0);
    assert_eq!(fence.seals.load(Ordering::SeqCst), 0);
}
#[test]
fn deadline_boundaries_zero_legacy_and_overflow_are_safe() {
    let (db, mut req) = fixture();
    req.input.now_ns -= 1;
    assert_hold(&db, &req, OnlineBudget::default(), "MemberNotDue");
    req.input.now_ns += 1;
    assert!(matches!(
        db.retention_online_prepare(&req, OnlineBudget::default())
            .unwrap(),
        OnlineOutcome::Ready(_)
    ));
    req.input.now_ns += 1;
    assert!(matches!(
        db.retention_online_prepare(&req, OnlineBudget::default())
            .unwrap(),
        OnlineOutcome::Ready(_)
    ));
    req.policy.archived_delete_after_days = 0;
    assert_hold(&db, &req, OnlineBudget::default(), "DeletionDisabled");
    req.policy.archived_delete_after_days = u32::MAX;
    assert_hold(&db, &req, OnlineBudget::default(), "InvalidDeadline");
    req.policy = ThreadRetentionPolicy::default();
    req.input.threads[0].archived_at_s = Some(i64::MAX);
    assert_hold(&db, &req, OnlineBudget::default(), "MemberNotDue");
    req.input.threads[0].archived_at_s = None;
    // Legacy archives still go through plan C, not an immediate-delete shortcut.
    db.conn.execute("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES('q','ra','ra','r','{}')",[]).unwrap();
    assert_hold(&db, &req, OnlineBudget::default(), "UnansweredQuestion");
}
#[test]
fn activity_and_changed_closure_are_rechecked_after_preflight() {
    let (db, req) = fixture();
    assert!(matches!(
        db.retention_online_prepare(&req, OnlineBudget::default())
            .unwrap(),
        OnlineOutcome::Ready(_)
    ));
    let kind = event_bus::EventKind::from(event_bus::MessageEvent::MessageCompleted {
        run_id: "ra".into(),
        text: "recent".into(),
    });
    db.conn.execute("INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id) VALUES('s',1,0,?1,'Message',?2,'MessageCompleted','ra')",params![req.input.now_ns,serde_json::to_string(&kind).unwrap()]).unwrap();
    assert_hold(&db, &req, OnlineBudget::default(), "RecentActivity");
    db.conn.execute("DELETE FROM events", []).unwrap();
    db.conn.execute("INSERT INTO threads(thread_id,parent_thread_id,archived,created_at_ns,updated_at_ns,source) VALUES('b','a',1,0,0,'host')",[]).unwrap();
    assert_hold(&db, &req, OnlineBudget::default(), "ClosureChanged");
}
#[test]
fn multi_family_closure_is_all_or_nothing() {
    for protection in ["pin", "archive", "deadline"] {
        let (db, mut req) = fixture();
        db.conn.execute("INSERT INTO threads(thread_id,parent_thread_id,archived,created_at_ns,updated_at_ns,source) VALUES('b','a',1,0,0,'host')",[]).unwrap();
        req.expected.threads.push("b".into());
        let mut b = req.input.threads[0].clone();
        b.thread_id = "b".into();
        let reason = match protection {
            "pin" => {
                b.pinned = true;
                "Pinned"
            }
            "archive" => {
                b.archived = false;
                "MemberNotArchived"
            }
            _ => {
                b.archived_at_s = Some(1);
                "MemberNotDue"
            }
        };
        req.input.threads.push(b);
        assert_hold(&db, &req, OnlineBudget::default(), reason);
    }
}
#[test]
fn bounded_confirmation_publishes_only_after_commit_and_never_reads_owners() {
    let (db, req) = fixture();
    let fence = Fence::default();
    let result = confirm(&db.conn, &req, OnlineBudget::default(), &fence).unwrap();
    assert!(matches!(result, OnlineOutcome::Confirmed(_)));
    assert!(db.tombstones().unwrap().contains_run("ra"));
    assert_eq!(fence.published.load(Ordering::SeqCst), 1);
    assert_eq!(fence.seals.load(Ordering::SeqCst), 0);
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM reclaim_jobs", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn commit_failure_rolls_back_all_tombstones_and_does_not_publish() {
    let (db, req) = fixture();
    db.conn.execute_batch("CREATE TEMP TRIGGER fail_confirm BEFORE INSERT ON reclaim_jobs BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let fence = Fence::default();
    assert!(confirm(&db.conn, &req, OnlineBudget::default(), &fence).is_err());
    assert!(db.tombstones().unwrap().is_empty());
    assert_eq!(fence.published.load(Ordering::SeqCst), 0);
    assert_eq!(fence.seals.load(Ordering::SeqCst), 0);
}
#[test]
fn budgets_and_binding_refuse_before_any_partial_tombstone() {
    let (db, mut req) = fixture();
    assert_hold(
        &db,
        &req,
        OnlineBudget {
            members: 1,
            ..Default::default()
        },
        "ClosureMemberBudget",
    );
    assert_hold(
        &db,
        &req,
        OnlineBudget {
            edges: 1,
            ..Default::default()
        },
        "ClosureEdgeBudget",
    );
    assert_hold(
        &db,
        &req,
        OnlineBudget {
            side_effect_rows: 0,
            ..Default::default()
        },
        "SideEffectRowBudget",
    );
    assert_hold(
        &db,
        &req,
        OnlineBudget {
            side_effect_bytes: 1,
            ..Default::default()
        },
        "SideEffectByteBudget",
    );
    db.conn.execute("INSERT INTO tasks(id,status,created_at_ns,updated_at_ns,parent_run_id) VALUES('t','completed',0,0,'ra')",[]).unwrap();
    assert_hold(
        &db,
        &req,
        OnlineBudget {
            tasks: 0,
            ..Default::default()
        },
        "TaskBudget",
    );
    req.binding.sidebar_path = "/different".into();
    assert_hold(&db, &req, OnlineBudget::default(), "BindingMismatch");
}
#[test]
fn trigger_targets_and_protection_sources_have_keyed_query_plans() {
    let (db, _) = fixture();
    for (table, key) in [
        ("pr_auto_fix_state", "destination_thread_id"),
        ("pr_notices", "destination_thread_id"),
        ("lane_recovery_episodes", "destination_thread_id"),
        ("lane_recovery_episodes", "saved_root_run_id"),
        ("lane_handoffs", "source_run_id"),
        ("recovery_claim_members", "run_id"),
        ("restart_expected_members", "run_id"),
        ("recovery_members", "run_id"),
    ] {
        let sql = format!("EXPLAIN QUERY PLAN SELECT * FROM {table} WHERE {key}=?1 LIMIT 4097");
        let plan = db
            .conn
            .prepare(&sql)
            .unwrap()
            .query_map(["a"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("; ");
        assert!(
            plan.contains(&format!("SEARCH {table} USING")),
            "{table}: {plan}"
        );
        assert!(!plan.contains(&format!("SCAN {table}")), "{table}: {plan}");
    }
}

#[test]
fn adopted_member_limit_and_one_above_are_atomic() {
    let (db, mut req) = fixture();
    let limit = OnlineBudget::default().members;
    for i in 0..limit - 2 {
        let id = format!("extra-{i:04}");
        db.conn
            .execute("INSERT INTO thread_runs VALUES(?1,'a','ra',0)", [&id])
            .unwrap();
        req.expected.runs.push(id);
    }
    req.expected.runs.sort();
    assert!(matches!(
        db.retention_online_prepare(&req, OnlineBudget::default())
            .unwrap(),
        OnlineOutcome::Ready(_)
    ));
    db.conn
        .execute("INSERT INTO thread_runs VALUES('overflow','a','ra',0)", [])
        .unwrap();
    assert_hold(&db, &req, OnlineBudget::default(), "ClosureMemberBudget");
}
#[test]
fn failed_reclaim_family_does_not_starve_the_next_live_writer_page() {
    let (db, req) = fixture();
    confirm(&db.conn, &req, OnlineBudget::default(), &Fence::default()).unwrap();
    db.conn.execute_batch("CREATE TEMP TRIGGER fail_reclaim_a BEFORE UPDATE ON reclaim_jobs WHEN OLD.family_key='a' BEGIN SELECT RAISE(ABORT,'injected reclaim failure'); END;
        INSERT INTO thread_tombstones VALUES('b','b',0);
        INSERT INTO reclaim_jobs(family_key,state,cursor_json,created_at_ns,updated_at_ns) VALUES('b','db_pending','{}',0,0);").unwrap();
    let budget = crate::thread_retention::ReclaimBudget {
        max_transactions: 1,
    };
    let (failed, cursor) =
        super::super::reclaim::reclaim_step_rotating(&db.conn, budget, None).unwrap();
    assert!(failed.last_error.is_some());
    assert_eq!(cursor.as_deref(), Some("a"));
    let (advanced, next) =
        super::super::reclaim::reclaim_step_rotating(&db.conn, budget, cursor.as_deref()).unwrap();
    assert!(advanced.last_error.is_none());
    assert_eq!(advanced.transactions, 1);
    assert_eq!(next.as_deref(), Some("b"));
}

#[test]
fn changed_database_identity_is_rejected_inside_confirmation() {
    let (db, req) = fixture();
    db.conn
        .execute("UPDATE retention_identity SET db_id='other' WHERE id=1", [])
        .unwrap();
    assert_hold(
        &db,
        &req,
        OnlineBudget::default(),
        "DatabaseIdentityMismatch",
    );
}

#[test]
fn initial_reclaim_page_includes_an_empty_sql_family_key() {
    let db = Database::open_in_memory().unwrap();
    db.conn.execute("INSERT INTO reclaim_jobs(family_key,state,cursor_json,created_at_ns,updated_at_ns) VALUES('','db_pending','{}',0,0)", []).unwrap();
    let (_, cursor) = crate::repo::thread_retention::reclaim::reclaim_step_rotating(
        &db.conn,
        crate::thread_retention::ReclaimBudget::default(),
        None,
    )
    .unwrap();
    assert_eq!(cursor.as_deref(), Some(""));
}

#[test]
fn supplemental_protection_must_clear_before_confirmation() {
    let (db, req) = fixture();
    db.conn.execute_batch("INSERT INTO restart_transactions(restart_id,capture_id,revision,phase,metadata_json,old_builtin_tool_names_json) VALUES('restart','capture',0,'captured','{}','[]');
        INSERT INTO restart_expected_members(restart_id,run_id,root_run_id,member_json) VALUES('restart','ra','ra','{}');").unwrap();
    assert_hold(&db, &req, OnlineBudget::default(), "RestartEvidence");
    db.conn
        .execute("UPDATE restart_transactions SET phase='completed'", [])
        .unwrap();
    assert!(matches!(
        db.retention_online_prepare(&req, OnlineBudget::default())
            .unwrap(),
        OnlineOutcome::Ready(_)
    ));

    db.conn.execute_batch("INSERT INTO recovery_manifests(manifest_id,schema_version,committed,member_count,edge_count,edges_json) VALUES('manifest',1,1,1,0,'[]');
        INSERT INTO recovery_members(manifest_id,run_id,incarnation,intent_revision,resume,record_json) VALUES('manifest','ra','old',0,1,'{}');
        INSERT INTO recovery_claims(claim_id,manifest_id,state) VALUES('claim','manifest','prepared');
        INSERT INTO recovery_claim_members(claim_id,run_id,old_incarnation,reserved_revision,new_incarnation) VALUES('claim','ra','old',0,'new');").unwrap();
    assert_hold(&db, &req, OnlineBudget::default(), "PreparedRecoveryClaim");
    db.conn
        .execute("UPDATE recovery_claims SET state='released'", [])
        .unwrap();
    assert!(matches!(
        db.retention_online_prepare(&req, OnlineBudget::default())
            .unwrap(),
        OnlineOutcome::Ready(_)
    ));

    db.conn.execute_batch("INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,request_hash,scope,policy,management_revision,management_projection_revision,brief_json,brief_revision,start_seed,root_run_id,generation,created_at_ns,updated_at_ns,last_activity_at_ns) VALUES('lane','a','a','ra','creation','hash','lane','detached',3,1,'{}',1,'{}','ra',1,0,0,0);").unwrap();
    assert_hold(
        &db,
        &req,
        OnlineBudget::default(),
        "MembershipProjectionPending",
    );
    db.conn
        .execute("UPDATE lanes SET management_projection_revision=3", [])
        .unwrap();
    assert!(matches!(
        confirm(&db.conn, &req, OnlineBudget::default(), &Fence::default()).unwrap(),
        OnlineOutcome::Confirmed(_)
    ));
}

#[test]
fn committed_manifest_links_sidebar_families_but_superseded_manifest_does_not() {
    for superseded in [false, true] {
        let (db, mut req) = fixture();
        db.conn.execute_batch("INSERT INTO threads(thread_id,root_run_id,archived,created_at_ns,updated_at_ns,source) VALUES('b','rb',1,0,0,'host');
            INSERT INTO thread_runs VALUES('rb','b','rb',0);
            INSERT INTO run_contexts(run_id,role,name,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) VALUES('rb','worker','w','{}','[]','done',1,0);
            INSERT INTO recovery_manifests(manifest_id,schema_version,committed,member_count,edge_count,edges_json) VALUES('manifest',1,1,2,0,'[]');
            INSERT INTO recovery_members(manifest_id,run_id,incarnation,intent_revision,resume,record_json) VALUES('manifest','ra','old',0,1,'{}'),('manifest','rb','old',0,1,'{}');").unwrap();
        db.conn
            .execute("UPDATE recovery_manifests SET superseded=?1", [superseded])
            .unwrap();
        req.expected = match db
            .retention_online_closure("a", OnlineBudget::default())
            .unwrap()
        {
            OnlineOutcome::Ready(closure) => closure,
            other => panic!("{other:?}"),
        };
        let mut b = req.input.threads[0].clone();
        b.thread_id = "b".into();
        b.pinned = true;
        req.input.threads.push(b);
        if !superseded {
            assert_eq!(req.expected.threads, ["a", "b"]);
            assert_hold(&db, &req, OnlineBudget::default(), "Pinned");
            req.input.threads[1].pinned = false;
        } else {
            assert_eq!(req.expected.threads, ["a"]);
        }
        assert!(matches!(
            confirm(&db.conn, &req, OnlineBudget::default(), &Fence::default()).unwrap(),
            OnlineOutcome::Confirmed(_)
        ));
        let tombstones = db.tombstones().unwrap();
        assert!(tombstones.contains_run("ra"));
        assert_eq!(tombstones.contains_run("rb"), !superseded);
    }
}
