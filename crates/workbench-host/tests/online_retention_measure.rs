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
        for targets in [0, 500] {
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
                for i in 0..unrelated {
                    tx.execute("INSERT INTO threads(thread_id,title,archived,created_at_ns,updated_at_ns,source) VALUES(?1,?2,0,0,0,'host')",
                params![format!("other-{i}"), "x".repeat(100)]).unwrap();
                }
                tx.execute("INSERT INTO threads(thread_id,root_run_id,archived,created_at_ns,updated_at_ns,source) VALUES('candidate','run-0',1,0,0,'host')", []).unwrap();
                for i in 0..budget.members - 1 {
                    let id = format!("run-{i}");
                    tx.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,'candidate','run-0',0)", [&id]).unwrap();
                    tx.execute("INSERT INTO run_contexts(run_id,role,name,parent_run_id,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) VALUES(?1,'worker','w',?2,'{}','[]','done',1,0)", params![id, (i!=0).then_some("run-0")]).unwrap();
                }
                for i in 0..budget.tasks {
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
                assert!(matches!(
                    measure("common preflight", || db
                        .retention_online_prepare(&request, budget)
                        .unwrap()),
                    OnlineOutcome::Ready(_)
                ));
                let timing = Arc::new(Timing::default());
                println!(
                    "fixture threads={unrelated}, closure={}, tasks={}, trigger_state={targets}, trial={trial}",
                    budget.members, budget.tasks
                );
                let result = measure(
                    "writer confirm including empty-queue round trip (occupancy upper bound)",
                    || {
                        handle
                            .confirm_retention_online(&authority, request, budget, timing.clone())
                            .unwrap()
                    },
                );
                assert!(matches!(result, OnlineOutcome::Confirmed(_)), "{result:?}");
                println!(
                    "tombstone+trigger+commit seal {:.3}ms",
                    timing.seals.lock().unwrap()[0].as_secs_f64() * 1000.0
                );
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
