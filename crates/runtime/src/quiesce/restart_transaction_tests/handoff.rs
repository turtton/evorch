use super::*;
use crate::AgentRuntime;
use crate::learning_jobs::*;
use std::sync::Arc;

fn origin(instance: &str) -> LearningOrigin {
    let mut origin = current_origin(LearningOriginKind::Core, Some("config".into()));
    origin.instance = instance.into();
    origin
}
fn learning(runtime: AgentRuntime, f: &Fixture, instance: &str) -> AgentRuntime {
    learning_with_origin(runtime, f, origin(instance))
}
fn learning_with_origin(
    runtime: AgentRuntime,
    f: &Fixture,
    origin: LearningOrigin,
) -> AgentRuntime {
    runtime
        .with_learning_origin(origin)
        .with_learning(crate::memory_queue::LearningSettings {
            writer: f._storage.handle(),
            storage: f.config.clone(),
            project: "p".into(),
            quick: crate::ModelPreference {
                profile: "test".into(),
                model: None,
                reasoning_effort: None,
            },
        })
}
fn successor(f: &Fixture) -> AgentRuntime {
    let bus = Arc::new(event_bus::EventBus::new(64));
    learning(
        AgentRuntime::new(
            bus.clone(),
            Arc::new(::tools::ToolExecutor::new(bus)),
            f.model.clone(),
        )
        .with_run_store(crate::RunStore::open(&f.config, f._storage.handle()).unwrap()),
        f,
        "successor",
    )
}
fn step(f: &Fixture, mutation: RestartMutation) -> RestartTransaction {
    f._storage
        .handle()
        .mutate_restart("restart".into(), Some(tx(f).revision), mutation)
        .unwrap()
}
fn adopt(_f: &Fixture, runtime: &AgentRuntime) {
    runtime
        .adopt_restart_handoff(core_instance::test_support::inherited_handoff_proof(
            "restart".into(),
            "config".into(),
            2,
            "nonce".into(),
            "new".into(),
        ))
        .unwrap();
}
fn admit(queue: &LearningJobQueue, id: &str) {
    queue
        .admit(AdmitRequest {
            task_id: id.into(),
            roots: vec![id.into()],
            goal: false,
            daily_limit: 20,
            project: "p".into(),
            source_run_id: id.into(),
            prompt: "fixed".into(),
            evidence_json: "{}".into(),
        })
        .unwrap();
}
struct Probe(OriginLiveness);
impl OriginProbe for Probe {
    fn liveness(&self, _: &LearningOrigin) -> OriginLiveness {
        self.0
    }
}

#[tokio::test]
async fn inherited_learning_uses_membership_not_pid_and_cold_completed_graph_cannot_resume() {
    let mut f = Fixture::new(0);
    f.runtime = learning(f.runtime.clone(), &f, "predecessor");
    let queue = f.runtime.learning_job_queue().unwrap();
    admit(&queue, "interrupted");
    let started = queue
        .start_attempt(
            "interrupted",
            &f.runtime.learning_gate().try_enter().unwrap(),
        )
        .unwrap()
        .unwrap();
    let pause = f.runtime.pause_learning();
    let suspension = f.runtime.suspend_learning(&pause).unwrap();
    suspension.wait().await.unwrap();
    // This job has no running entry: represent its already-normal returned tail explicitly.
    queue
        .change(
            &started,
            LearningAttemptChange::Interrupt {
                suspension_id: suspension.id().into(),
            },
        )
        .unwrap();
    // Final inventory must also include admitted Pending jobs without creating fake attempts.
    admit(&queue, "pending");
    let mut session = session(&f, BTreeMap::new()).await;
    f.runtime
        .persist_restart_capture_expected(&mut session)
        .unwrap();
    f._storage
        .handle()
        .inject_restart_write_fault(true)
        .unwrap();
    let inventory = f
        .runtime
        .persist_restart_learning_inventory(&mut session, &pause, &suspension)
        .await
        .unwrap();
    assert_eq!(inventory.members.len(), 2);
    f.runtime.commit_restart_capture(&mut session).unwrap();
    step(
        &f,
        RestartMutation::Arm {
            nonce_hash: "nonce".into(),
        },
    );
    let successor = successor(&f);
    assert_eq!(origin("predecessor").pid, successor.learning_origin().pid);
    // Equal PID/start time and even an Adopted DB row are not recovery authority.
    step(
        &f,
        RestartMutation::Adopt {
            successor_epoch: 2,
            config_identity: "config".into(),
            successor_build: "new".into(),
            nonce_hash: "nonce".into(),
        },
    );
    assert!(successor.resume_planned_learning().is_err());
    adopt(&f, &successor);
    assert!(
        successor.resume_planned_learning().is_err(),
        "Ready must gate requeue"
    );
    step(&f, RestartMutation::Ready { at_ms: 1 });
    f._storage
        .handle()
        .inject_restart_write_fault(true)
        .unwrap();
    let results = successor.resume_planned_learning().unwrap();
    assert!(results.iter().all(|s| s.disposition == "requeued"));
    assert_eq!(successor.resume_planned_learning().unwrap(), results);
    let job = queue.job("interrupted").unwrap().unwrap();
    assert_eq!(job.attempt, 1);
    assert_eq!(job.generation, 2);
    assert_eq!(job.origin, successor.learning_origin());
    assert_eq!(queue.job("pending").unwrap().unwrap().attempt, 0);
    assert!(
        successor
            .quarantine_cold_learning(&Probe(OriginLiveness::Dead))
            .is_err()
    );
    // A later cold boot must not let an old, already-requeued membership hide
    // a fresh Interrupted attempt from the origin-aware standalone cleanup.
    let next_queue = successor.learning_job_queue().unwrap();
    let fresh = next_queue
        .start_attempt(
            "interrupted",
            &successor.learning_gate().try_enter().unwrap(),
        )
        .unwrap()
        .unwrap();
    next_queue
        .change(
            &fresh,
            LearningAttemptChange::Interrupt {
                suspension_id: "later".into(),
            },
        )
        .unwrap();
    let bus = Arc::new(event_bus::EventBus::new(64));
    let cold = AgentRuntime::new(
        bus.clone(),
        Arc::new(::tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, f._storage.handle()).unwrap())
    .with_learning_origin(origin("cold-later"));
    cold.quarantine_cold_restarts().unwrap();
    cold.quarantine_cold_learning(&Probe(OriginLiveness::Dead))
        .unwrap();
    assert_eq!(
        queue.job("interrupted").unwrap().unwrap().state,
        LearningJobState::Abandoned
    );
}

#[tokio::test]
async fn planned_learning_dispatch_opens_only_after_ready_and_starts_a_fresh_attempt_and_run() {
    let mut f = Fixture::new(0);
    f.runtime = learning(f.runtime.clone(), &f, "predecessor");
    let queue = f.runtime.learning_job_queue().unwrap();
    let source = RunId::new(998);
    queue
        .admit(AdmitRequest {
            task_id: "fresh".into(),
            roots: vec![source.to_string()],
            goal: false,
            daily_limit: 20,
            project: "p".into(),
            source_run_id: source.to_string(),
            prompt: "fixed".into(),
            evidence_json: r#"{"runs":{}}"#.into(),
        })
        .unwrap();
    let started = queue
        .start_attempt("fresh", &f.runtime.learning_gate().try_enter().unwrap())
        .unwrap()
        .unwrap();
    let pause = f.runtime.pause_learning();
    let suspension = f.runtime.suspend_learning(&pause).unwrap();
    suspension.wait().await.unwrap();
    queue
        .change(
            &started,
            LearningAttemptChange::Interrupt {
                suspension_id: suspension.id().into(),
            },
        )
        .unwrap();
    let mut session = session(&f, BTreeMap::new()).await;
    f.runtime
        .persist_restart_capture_expected(&mut session)
        .unwrap();
    f.runtime
        .persist_restart_learning_inventory(&mut session, &pause, &suspension)
        .await
        .unwrap();
    f.runtime.commit_restart_capture(&mut session).unwrap();
    step(
        &f,
        RestartMutation::Arm {
            nonce_hash: "nonce".into(),
        },
    );
    let runtime = successor(&f);
    let successor_pause = runtime.pause_learning();
    adopt(&f, &runtime);
    assert!(runtime.resume_planned_learning().is_err());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    step(&f, RestartMutation::Ready { at_ms: 1 });
    runtime.resume_planned_learning().unwrap();
    assert_eq!(
        queue.job("fresh").unwrap().unwrap().state,
        LearningJobState::Pending
    );
    assert!(runtime.resume_learning(successor_pause));
    f.model.entered.notified().await;
    let job = queue.job("fresh").unwrap().unwrap();
    assert_eq!(job.state, LearningJobState::Started);
    assert_eq!(job.attempt, 2);
    assert_eq!(job.generation, 3); // Requeue and the fresh Start each fence older writes.
    let active = runtime.list_agents();
    assert_eq!(active.len(), 1);
    assert_ne!(active[0].run_id, source);
    runtime.cancel(active[0].run_id).unwrap();
    f.model.release.add_permits(1);
    runtime.drain_learning().await;
    // This fixture submits no lesson: the attempt terminates without promoting evidence.
    assert!(queue.job("fresh").unwrap().unwrap().state.is_terminal());
}

#[derive(Clone, Copy, Debug)]
enum ColdGraphPath {
    Minimal,
    Mutation,
    Completed,
}

async fn cold_learning_after_graph(liveness: OriginLiveness, other_config: bool) {
    for path in [
        ColdGraphPath::Minimal,
        ColdGraphPath::Mutation,
        ColdGraphPath::Completed,
    ] {
        let mut f = Fixture::new(0);
        f.runtime = learning(f.runtime.clone(), &f, "predecessor");
        let queue = f.runtime.learning_job_queue().unwrap();
        admit(&queue, "pending");
        admit(&queue, "interrupted");
        let started = queue
            .start_attempt(
                "interrupted",
                &f.runtime.learning_gate().try_enter().unwrap(),
            )
            .unwrap()
            .unwrap();
        let pause = f.runtime.pause_learning();
        let suspension = f.runtime.suspend_learning(&pause).unwrap();
        suspension.wait().await.unwrap();
        // This manually started job has no running tail; persist its normal interruption.
        queue
            .change(
                &started,
                LearningAttemptChange::Interrupt {
                    suspension_id: suspension.id().into(),
                },
            )
            .unwrap();
        let mut session = session(&f, BTreeMap::new()).await;
        f.runtime
            .persist_restart_capture_expected(&mut session)
            .unwrap();
        f.runtime
            .persist_restart_learning_inventory(&mut session, &pause, &suspension)
            .await
            .unwrap();
        f.runtime.commit_restart_capture(&mut session).unwrap();
        step(
            &f,
            RestartMutation::Arm {
                nonce_hash: "nonce".into(),
            },
        );
        if matches!(path, ColdGraphPath::Completed) {
            let planned = successor(&f);
            adopt(&f, &planned);
            step(&f, RestartMutation::Ready { at_ms: 1 });
            assert_eq!(tx(&f).phase, RestartPhase::Completed); // Crash before learning requeue.
        }
        let foreign = LearningJobQueue::new(
            f._storage.handle(),
            f.config.clone(),
            current_origin(LearningOriginKind::Gui, None),
        );
        admit(&foreign, "foreign");
        let before = ["pending", "interrupted", "foreign"].map(|id| {
            (
                queue.job(id).unwrap().unwrap(),
                queue.evidence(id).unwrap().unwrap(),
            )
        });
        let mut cold_origin = origin("cold");
        if other_config {
            cold_origin.config_identity = Some("other-config".into());
        }
        let bus = Arc::new(event_bus::EventBus::new(64));
        let cold = AgentRuntime::new(
            bus.clone(),
            Arc::new(::tools::ToolExecutor::new(bus)),
            f.model.clone(),
        )
        .with_run_store(crate::RunStore::open(&f.config, f._storage.handle()).unwrap());
        let cold = if matches!(path, ColdGraphPath::Completed) {
            cold.with_learning_origin(cold_origin)
        } else {
            learning_with_origin(cold, &f, cold_origin)
        };
        // Follow the startup order: graph quarantine first, then the origin-aware learning pass.
        match path {
            ColdGraphPath::Mutation => {
                step(&f, RestartMutation::QuarantineCold);
            }
            ColdGraphPath::Minimal | ColdGraphPath::Completed => {
                cold.quarantine_cold_restarts().unwrap();
            }
        }
        assert_eq!(
            tx(&f).phase,
            if matches!(path, ColdGraphPath::Completed) {
                RestartPhase::Completed
            } else {
                RestartPhase::Invalidated
            }
        );
        for (job, evidence) in &before {
            assert_eq!(
                queue.job(&job.task_id).unwrap().as_ref(),
                Some(job),
                "{path:?}: graph changed learning"
            );
            assert_eq!(
                queue.evidence(&job.task_id).unwrap().as_ref(),
                Some(evidence),
                "{path:?}: graph removed evidence"
            );
        }
        // Graph isolation has no liveness evidence and must not consume learning membership.
        assert!(
            db(&f)
                .restart_learning_members("restart")
                .unwrap()
                .iter()
                .all(|member| member.disposition == "pending")
        );
        cold.quarantine_cold_learning(&Probe(liveness)).unwrap();
        for (job, evidence) in before {
            if job.origin.kind == LearningOriginKind::Core
                && !other_config
                && matches!(liveness, OriginLiveness::Dead)
            {
                assert_eq!(
                    queue.job(&job.task_id).unwrap().unwrap().state,
                    LearningJobState::Abandoned,
                    "{path:?}"
                );
                assert!(queue.evidence(&job.task_id).unwrap().is_none(), "{path:?}");
            } else {
                assert_eq!(
                    queue.job(&job.task_id).unwrap().as_ref(),
                    Some(&job),
                    "{path:?}: protected origin changed"
                );
                assert_eq!(
                    queue.evidence(&job.task_id).unwrap().as_ref(),
                    Some(&evidence),
                    "{path:?}: protected evidence removed"
                );
            }
        }
    }
}

#[tokio::test]
async fn cold_graph_then_learning_preserves_alive_origins_and_evidence() {
    cold_learning_after_graph(OriginLiveness::Alive, false).await;
}

#[tokio::test]
async fn cold_graph_then_learning_preserves_unknown_origins_and_evidence() {
    cold_learning_after_graph(OriginLiveness::Unknown, false).await;
}

#[tokio::test]
async fn cold_graph_then_learning_preserves_other_config_and_evidence() {
    cold_learning_after_graph(OriginLiveness::Dead, true).await;
}

#[tokio::test]
async fn cold_graph_then_learning_ends_only_dead_same_config_core_jobs() {
    cold_learning_after_graph(OriginLiveness::Dead, false).await;
}

#[tokio::test]
async fn boot_receipt_adapter_records_diagnostics_without_granting_adoption() {
    use core_instance::handoff::{boot::BootFailure, receipt::*};
    let f = Fixture::new(0);
    let mut session = session(&f, BTreeMap::new()).await;
    f.runtime
        .persist_restart_capture_expected(&mut session)
        .unwrap();
    f.runtime.commit_restart_capture(&mut session).unwrap();
    step(
        &f,
        RestartMutation::Arm {
            nonce_hash: "nonce".into(),
        },
    );
    let mut receipt = RestartBootReceipt {
        version: 1,
        config_identity: "config".into(),
        transaction_id: "restart".into(),
        predecessor_epoch: 1,
        successor_epoch: 2,
        pid: 42,
        process_start: ProcessStart {
            boot_id: "boot".into(),
            start_ticks: 7,
        },
        confirmed_phase: Some(BootPhase::Armed),
        status: BootStatus::Restarting,
        revision: 1,
        next_action: NextAction::WaitForReady,
    };
    let runtime = successor(&f);
    let first = runtime.record_restart_boot_receipt(&receipt, 123).unwrap();
    assert_eq!(
        runtime.record_restart_boot_receipt(&receipt, 123).unwrap(),
        first
    );
    assert!(runtime.adopted_restart_transaction().is_err());
    assert_eq!(tx(&f).phase, RestartPhase::Armed);
    receipt.config_identity = "stale".into();
    assert!(runtime.record_restart_boot_receipt(&receipt, 123).is_err());
    receipt.config_identity = "config".into();
    receipt.revision = 2;
    receipt.status = BootStatus::Failed {
        failure: BootFailure::AdoptAckUnknown,
        exit_code: 72,
    };
    receipt.next_action = NextAction::ExplicitColdStart;
    assert_eq!(
        runtime
            .record_restart_boot_receipt(&receipt, 123)
            .unwrap()
            .exit_code,
        Some(72)
    );
    assert_eq!(tx(&f).phase, RestartPhase::Armed);
}

#[tokio::test]
async fn empty_learning_inventory_requires_a_durable_row_not_an_empty_members_query() {
    let mut f = Fixture::new(0);
    f.runtime = learning(f.runtime.clone(), &f, "predecessor");
    let pause = f.runtime.pause_learning();
    let suspension = f.runtime.suspend_learning(&pause).unwrap();
    let mut session = session(&f, BTreeMap::new()).await;
    f.runtime
        .persist_restart_capture_expected(&mut session)
        .unwrap();
    f._storage
        .handle()
        .inject_restart_write_fault(false)
        .unwrap();
    assert!(
        f.runtime
            .persist_restart_learning_inventory(&mut session, &pause, &suspension)
            .await
            .is_err()
    );
    assert!(
        db(&f)
            .restart_saved_learning_inventory("restart")
            .unwrap()
            .is_none()
    );
    f._storage
        .handle()
        .inject_restart_write_fault(true)
        .unwrap();
    assert!(
        f.runtime
            .persist_restart_learning_inventory(&mut session, &pause, &suspension)
            .await
            .unwrap()
            .members
            .is_empty()
    );
    f.runtime.commit_restart_capture(&mut session).unwrap();
}

#[tokio::test]
async fn corrupt_cold_learning_cohort_never_blocks_a_later_valid_cohort() {
    for corruption in ["graph", "inventory", "member"] {
        let mut f = Fixture::new(0);
        f.runtime = learning(f.runtime.clone(), &f, "predecessor");
        let queue = f.runtime.learning_job_queue().unwrap();
        admit(&queue, "valid");
        let pause = f.runtime.pause_learning();
        let suspension = f.runtime.suspend_learning(&pause).unwrap();
        suspension.wait().await.unwrap();
        let mut session = session(&f, BTreeMap::new()).await;
        f.runtime
            .persist_restart_capture_expected(&mut session)
            .unwrap();
        f.runtime
            .persist_restart_learning_inventory(&mut session, &pause, &suspension)
            .await
            .unwrap();
        f.runtime.commit_restart_capture(&mut session).unwrap();
        // Independent earlier cohort: malformed persisted evidence is not authority.
        let mut meta = metadata();
        meta.restart_id = "a-corrupt".into();
        meta.capture_id = "a-capture".into();
        f._storage
            .handle()
            .mutate_restart(
                "a-corrupt".into(),
                None,
                RestartMutation::Create(Box::new(meta)),
            )
            .unwrap();
        let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
        sql.execute("INSERT INTO restart_learning_inventory SELECT 'a-corrupt', inventory_json FROM restart_learning_inventory WHERE restart_id='restart'", []).unwrap();
        if corruption == "graph" {
            sql.execute(
                "UPDATE restart_transactions SET metadata_json='{' WHERE restart_id='a-corrupt'",
                [],
            )
            .unwrap();
        } else if corruption == "inventory" {
            sql.execute("UPDATE restart_learning_inventory SET inventory_json='{' WHERE restart_id='a-corrupt'", []).unwrap();
        } else {
            sql.execute("INSERT INTO restart_learning_members (restart_id,task_id,member_json) VALUES ('a-corrupt','valid','{')", []).unwrap();
        }
        let bus = Arc::new(event_bus::EventBus::new(64));
        let cold = AgentRuntime::new(
            bus.clone(),
            Arc::new(::tools::ToolExecutor::new(bus)),
            f.model.clone(),
        )
        .with_run_store(crate::RunStore::open(&f.config, f._storage.handle()).unwrap())
        .with_learning_origin(origin("cold")); // No quick profile required for cleanup.
        cold.quarantine_cold_restarts().unwrap();
        cold.quarantine_cold_learning(&Probe(OriginLiveness::Dead))
            .unwrap();
        assert_eq!(
            queue.job("valid").unwrap().unwrap().state,
            LearningJobState::Abandoned
        );
        assert!(queue.evidence("valid").unwrap().is_none());
        assert!(cold.list_agents().is_empty());
        assert!(
            db(&f)
                .restart_learning_members("restart")
                .unwrap()
                .iter()
                .all(|m| m.disposition == "quarantined")
        );
    }
}
