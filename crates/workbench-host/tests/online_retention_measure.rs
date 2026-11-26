//! Manual host-side performance harness. Never asserts on wall-clock time.
//! Initial declared targets: writer command, seal, host lock and foreground
//! additional delay <100ms observed max. Fix budgets/indexes on an exceedance.
//! Run with cargo test --offline -p workbench-host --test online_retention_measure
//! -- --ignored --nocapture. Only synthetic TempDir data is used.
use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use storage::thread_retention::{online::*, *};

#[derive(Default)]
struct Timing {
    seals: Arc<Mutex<Vec<Duration>>>,
}
struct Seal {
    at: Instant,
    seals: Arc<Mutex<Vec<Duration>>>,
}
impl CommitFence for Timing {
    fn try_seal(&self) -> Option<Box<dyn SealedCommit>> {
        Some(Box::new(Seal {
            at: Instant::now(),
            seals: self.seals.clone(),
        }))
    }
}
impl SealedCommit for Seal {
    fn publish(&mut self, _: &ConfirmedFamily) {}
}
impl Drop for Seal {
    fn drop(&mut self) {
        self.seals.lock().unwrap().push(self.at.elapsed());
    }
}
const DAY: i64 = NANOS_PER_DAY;
fn measure<T>(label: &str, work: impl FnOnce() -> T) -> T {
    let at = Instant::now();
    let value = work();
    println!("{label}: {:.3}ms", at.elapsed().as_secs_f64() * 1000.0);
    value
}

// Exercise the actual common decision under its default row/decode budgets,
// not only tombstone trigger fanout. Whitespace is valid JSON and charged bytes.
fn seed_decision_budget(tx: &rusqlite::Transaction<'_>, profile: &str) {
    use event_bus::{
        AgentRunPhase, EventKind, LifecycleEvent, OrchestratorEvent, ThreadGoalPhase,
        ThreadGoalSnapshot, ThreadGoalUsage,
    };
    if profile == "goals" {
        let kind: EventKind = OrchestratorEvent::ThreadGoalUpdated {
            snapshot: ThreadGoalSnapshot {
                goal_id: "goal".into(),
                thread_id: "candidate".into(),
                root_run_id: "run-0".into(),
                related_root_run_ids: vec![],
                objective: "x".repeat(20_000),
                criteria: vec![],
                checks: vec![],
                phase: ThreadGoalPhase::Complete,
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
                original_request: String::new(),
            },
        }
        .into();
        tx.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Orchestrator','ThreadGoalUpdated','run-0',0,?1)", [serde_json::to_string(&kind).unwrap()]).unwrap();
        for i in 0..127 {
            let mut payload = r#"{"goal_id":"different"}"#.to_owned();
            payload.push_str(&" ".repeat(6_000 - payload.len()));
            tx.execute("INSERT INTO lane_handoffs(source_run_id,source_incarnation,target_run_id,lane_id,request_json,state,goal_json) VALUES('run-0',?1,?2,?3,'{}','committed',?4)",
                params![format!("inc-{i}"),format!("historical-{i}"),format!("missing-lane-{i}"),payload]).unwrap();
        }
        println!(
            "decision fixture goal snapshot=1, handoff receipts=127/128, receipt bytes=762000"
        );
    } else {
        let bytes = if profile == "bytes-over" {
            4_100
        } else {
            4_096
        };
        for i in 0..32 {
            let kind: EventKind = LifecycleEvent::AgentRunStateChanged {
                run_id: format!("run-{i}"),
                from: AgentRunPhase::Waiting,
                to: AgentRunPhase::Stopped,
                reason: None,
                waiting: None,
            }
            .into();
            let mut payload = serde_json::to_string(&kind).unwrap();
            payload.push_str(&" ".repeat(bytes - payload.len()));
            assert!(serde_json::from_str::<EventKind>(&payload).is_ok());
            for _ in 0..16 {
                tx.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Lifecycle','AgentRunStateChanged',?1,?2,?3)",
                    params![format!("run-{i}"),14*DAY,payload]).unwrap();
            }
        }
        println!("decision fixture P=16, Q=512, bytes={}", 512 * bytes);
    }
}

#[test]
#[ignore = "manual synthetic benchmark, not a timing correctness test"]
fn online_retention_writer_budget_measurement() {
    let budget = OnlineBudget::default();
    println!(
        "initial candidate budget {budget:?}; warm page cache; optimized={}",
        !cfg!(debug_assertions)
    );
    let trials = std::env::var("RETENTION_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    for unrelated in [1_000, 2_000] {
        for (targets, decision_profile) in [
            (0, None),
            (500, None),
            (0, Some("tail")),
            (0, Some("bytes-over")),
            (0, Some("goals")),
        ] {
            for trial in 0..trials {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().join("root");
                std::fs::create_dir(&root).unwrap();
                let sidebar = dir.path().join("sidebar.json");
                std::fs::write(&sidebar, "{}").unwrap();
                let config = storage::StorageConfig {
                    db_path: dir.path().join("events.db"),
                    ..Default::default()
                };
                drop(storage::Database::open(&config).unwrap());
                let conn = Connection::open(&config.db_path).unwrap();
                let tx = conn.unchecked_transaction().unwrap();
                let task_count = if decision_profile.is_none() {
                    budget.tasks
                } else {
                    0
                };
                for i in 0..unrelated {
                    tx.execute("INSERT INTO threads(thread_id,title,archived,created_at_ns,updated_at_ns,source) VALUES(?1,?2,0,0,0,'host')",
                params![format!("other-{i}"), "x".repeat(100)]).unwrap();
                    let run = format!("unrelated-history-{i}");
                    let kind: event_bus::EventKind = event_bus::MessageEvent::MessageCompleted {
                        run_id: run.clone(),
                        text: "unrelated history ".repeat(32),
                    }
                    .into();
                    let body = serde_json::to_string(&kind).unwrap();
                    for _ in 0..4 {
                        tx.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Message','MessageCompleted',?1,?2,?3)",
                            params![run,14*DAY,body]).unwrap();
                    }
                }
                tx.execute("INSERT INTO threads(thread_id,root_run_id,archived,created_at_ns,updated_at_ns,source) VALUES('candidate','run-0',1,0,0,'host')", []).unwrap();
                for i in 0..budget.members - 1 {
                    let id = format!("run-{i}");
                    tx.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,'candidate','run-0',0)", [&id]).unwrap();
                    tx.execute("INSERT INTO run_contexts(run_id,role,name,parent_run_id,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) VALUES(?1,'worker','w',?2,'{}','[]','done',1,0)", params![id, (i!=0).then_some("run-0")]).unwrap();
                }
                for i in 0..task_count {
                    tx.execute("INSERT INTO tasks(id,status,created_at_ns,updated_at_ns,parent_run_id) VALUES(?1,'completed',0,0,'run-0')", [format!("task-{i}")]).unwrap();
                }
                for i in 0..targets {
                    tx.execute("INSERT INTO pr_auto_fix_state(pr_key,destination_thread_id) VALUES(?1,'candidate')", [format!("pr-{i}")]).unwrap();
                }
                if targets > 0 {
                    for i in 0..56 {
                        tx.execute("INSERT INTO pr_notices(notice_id,pr_key,head_sha,mode,destination_thread_id,link_revision,policy_revision,budget_epoch,success_seq,phase,payload) VALUES(?1,?2,'sha','auto','candidate',1,1,0,0,'ready',?3)", params![format!("notice-{i}"),format!("pr-{i}"),"x".repeat(15_000)]).unwrap();
                    }
                }
                if let Some(profile) = decision_profile {
                    seed_decision_budget(&tx, profile);
                }
                tx.commit().unwrap();
                drop(conn);
                let leases =
                    storage::WriterLeases::acquire(&config.db_path, &root, Duration::ZERO).unwrap();
                let owner = ExclusiveFixtureStorage::open(config.clone(), &root, &leases).unwrap();
                let handle = owner.handle();
                let authority = leases.authority().unwrap();
                let db = storage::Database::open(&config).unwrap();
                runtime::thread_retention::binding::ensure_binding(
                    &db,
                    &handle,
                    &authority,
                    runtime::thread_retention::binding::BindingPaths {
                        ownership_root: &root,
                        sidebar_path: &sidebar,
                    },
                    &config.db_path,
                    0,
                )
                .unwrap();
                let expected = match measure("closure prepare", || {
                    db.retention_online_closure("candidate", budget).unwrap()
                }) {
                    OnlineOutcome::Ready(c) => c,
                    other => panic!("{other:?}"),
                };
                let binding = BindingInput {
                    ownership_root: root.to_str().unwrap().into(),
                    sidebar_path: sidebar.to_str().unwrap().into(),
                };
                let request = OnlineRequest {
                    db_identity: db.retention_identity().unwrap(),
                    seed: "candidate".into(),
                    expected,
                    binding: binding.clone(),
                    policy: ThreadRetentionPolicy::default(),
                    input: RetentionInput {
                        threads: vec![SidebarThread {
                            thread_id: "candidate".into(),
                            archived: true,
                            archived_at_s: Some(0),
                            pinned: false,
                            updated_at_s: 0,
                            created_at_s: 0,
                            archive_revision: Some(1),
                            pin_revision: Some(0),
                        }],
                        ownership_root: binding.ownership_root,
                        sidebar_path: binding.sidebar_path,
                        now_ns: 14 * DAY,
                    },
                };
                let preflight = measure("common preflight", || {
                    db.retention_online_prepare(&request, budget).unwrap()
                });
                assert_eq!(
                    matches!(preflight, OnlineOutcome::Hold(_)),
                    decision_profile == Some("bytes-over"),
                    "{preflight:?}"
                );
                let timing = Arc::new(Timing::default());
                println!(
                    "fixture threads={unrelated}, closure={}, tasks={task_count}, trigger_state={targets}, decision={decision_profile:?}, trial={trial}",
                    budget.members
                );
                let result = measure(
                    "writer confirm including empty-queue round trip (occupancy upper bound)",
                    || {
                        handle
                            .confirm_retention_online(&authority, request, budget, timing.clone())
                            .unwrap()
                    },
                );
                if decision_profile == Some("bytes-over") {
                    assert!(
                        matches!(result, OnlineOutcome::Hold("ActivityProbeBudget")),
                        "{result:?}"
                    );
                    assert!(timing.seals.lock().unwrap().is_empty());
                    assert!(db.tombstones().unwrap().is_empty());
                    println!("decision hold: {result:?}; partial tombstones=0");
                } else {
                    assert!(matches!(result, OnlineOutcome::Confirmed(_)), "{result:?}");
                    println!(
                        "tombstone+trigger+commit seal {:.3}ms",
                        timing.seals.lock().unwrap()[0].as_secs_f64() * 1000.0
                    );
                }
                measure("online reclaim 1 transaction", || {
                    handle
                        .reclaim_step(ReclaimBudget {
                            max_transactions: 1,
                        })
                        .unwrap()
                });
                measure("incremental vacuum 1 step (measurement only)", || {
                    db.retention_vacuum_step(1).unwrap()
                });
                measure("maintenance complete", || handle.checkpoint_now().unwrap());
                measure("foreground baseline statistics", || {
                    handle.statistics().unwrap()
                });
            }
        }
    }
}
