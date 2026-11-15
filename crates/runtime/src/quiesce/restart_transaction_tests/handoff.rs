use super::*;
use crate::AgentRuntime;
use crate::learning_jobs::*;
use crate::ownership::{AdoptedOwnershipEvidence, PlannedHold};
use std::sync::Arc;

fn origin(instance: &str) -> LearningOrigin {
    let mut origin = current_origin(LearningOriginKind::Core, Some("config".into()));
    origin.instance = instance.into();
    origin
}
fn learning(runtime: AgentRuntime, f: &Fixture, instance: &str) -> AgentRuntime {
    runtime
        .with_learning_origin(origin(instance))
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

#[tokio::test]
async fn cold_learning_preserves_live_unknown_and_foreign_origins_but_ends_dead_members() {
    let mut f = Fixture::new(0);
    f.runtime = learning(f.runtime.clone(), &f, "predecessor");
    let queue = f.runtime.learning_job_queue().unwrap();
    admit(&queue, "pending");
    let pause = f.runtime.pause_learning();
    let suspension = f.runtime.suspend_learning(&pause).unwrap();
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
    let planned = successor(&f);
    adopt(&f, &planned);
    step(&f, RestartMutation::Ready { at_ms: 1 });
    assert_eq!(tx(&f).phase, RestartPhase::Completed); // Crash before learning requeue.
    let cold = successor(&f);
    cold.quarantine_cold_learning(&Probe(OriginLiveness::Alive))
        .unwrap();
    cold.quarantine_cold_learning(&Probe(OriginLiveness::Unknown))
        .unwrap();
    assert_eq!(
        queue.job("pending").unwrap().unwrap().state,
        LearningJobState::Pending
    );
    let foreign = LearningJobQueue::new(
        f._storage.handle(),
        f.config.clone(),
        current_origin(LearningOriginKind::Gui, None),
    );
    admit(&foreign, "foreign");
    cold.quarantine_cold_learning(&Probe(OriginLiveness::Dead))
        .unwrap();
    assert_eq!(
        queue.job("pending").unwrap().unwrap().state,
        LearningJobState::Abandoned
    );
    assert!(queue.evidence("pending").unwrap().is_none());
    assert_eq!(
        foreign.job("foreign").unwrap().unwrap().state,
        LearningJobState::Pending
    );
}

#[tokio::test]
async fn ownership_evidence_requires_physical_adoption_full_idle_inventory_and_current_readback() {
    let f = Fixture::new(0);
    let mut session = session(&f, BTreeMap::new()).await;
    f.runtime
        .persist_restart_capture_expected(&mut session)
        .unwrap();
    let progress = RestartOwnerProgress {
        thread_id: "idle".into(),
        old_owner_id: "owner".into(),
        old_generation: 7,
        active_runs: BTreeSet::new(),
        hold_saved: true,
        hold_read_back: true,
        worker_joined: true,
        socket_unlinked: true,
        hold_removed: false,
    };
    // Owner progress is a post-capture journal independent of session revision.
    f.runtime.commit_restart_capture(&mut session).unwrap();
    step(&f, RestartMutation::OwnerProgress(progress.clone()));
    step(
        &f,
        RestartMutation::Arm {
            nonce_hash: "nonce".into(),
        },
    );
    let runtime = successor(&f);
    assert!(runtime.adopted_restart_ownership_evidence().is_err());
    step(
        &f,
        RestartMutation::Adopt {
            successor_epoch: 2,
            config_identity: "config".into(),
            successor_build: "new".into(),
            nonce_hash: "nonce".into(),
        },
    );
    assert!(runtime.adopted_restart_ownership_evidence().is_err());
    adopt(&f, &runtime);
    let evidence = runtime.adopted_restart_ownership_evidence().unwrap();
    let hold = PlannedHold {
        transaction_id: "restart".into(),
        capture_id: "capture".into(),
        thread_id: "idle".into(),
        owner_id: "owner".into(),
        generation: 7,
        active_runs: BTreeSet::new(),
        orphaned: false,
    };
    evidence.validate_hold(&hold).unwrap();
    for wrong in [
        PlannedHold {
            generation: 8,
            ..hold.clone()
        },
        PlannedHold {
            thread_id: "not-in-inventory".into(),
            ..hold.clone()
        },
        PlannedHold {
            active_runs: BTreeSet::from(["not-captured".into()]),
            ..hold.clone()
        },
        PlannedHold {
            orphaned: true,
            ..hold.clone()
        },
    ] {
        assert!(evidence.validate_hold(&wrong).is_err());
    }
    step(
        &f,
        RestartMutation::OwnerProgress(RestartOwnerProgress {
            hold_removed: true,
            ..progress
        }),
    );
    assert!(
        evidence.validate_hold(&hold).is_err(),
        "each validation must re-read durable state"
    );
}

#[tokio::test]
async fn active_hold_cannot_survive_a_run_incarnation_change() {
    let f = Fixture::new(0);
    let owners = crate::ownership::OwnerHost::open(
        f._dir.sockets(),
        Default::default(),
        Arc::new(event_bus::EventBus::new(64)),
    )
    .unwrap();
    let permit = owners.start("thread").unwrap();
    let run = f
        .runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "park".into(),
            RunConfig {
                ownership: Some(permit.clone()),
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.model.entered.notified().await;
    let mut session = session(&f, BTreeMap::from([(run, recipe())])).await;
    f.runtime
        .persist_restart_capture_expected(&mut session)
        .unwrap();
    f.runtime.commit_restart_capture(&mut session).unwrap();
    let member = tx(&f).expected[0].clone();
    assert_eq!(member.thread_id.as_deref(), Some("thread"));
    step(
        &f,
        RestartMutation::OwnerProgress(RestartOwnerProgress {
            thread_id: "thread".into(),
            old_owner_id: permit.lease.owner_id.clone(),
            old_generation: permit.lease.generation,
            active_runs: BTreeSet::from([run.to_string()]),
            hold_saved: true,
            hold_read_back: true,
            worker_joined: true,
            socket_unlinked: true,
            hold_removed: false,
        }),
    );
    step(
        &f,
        RestartMutation::Arm {
            nonce_hash: "nonce".into(),
        },
    );
    let runtime = successor(&f);
    adopt(&f, &runtime);
    let evidence = runtime.adopted_restart_ownership_evidence().unwrap();
    let hold = PlannedHold {
        transaction_id: "restart".into(),
        capture_id: "capture".into(),
        thread_id: "thread".into(),
        owner_id: permit.lease.owner_id,
        generation: permit.lease.generation,
        active_runs: BTreeSet::from([run.to_string()]),
        orphaned: false,
    };
    evidence.validate_hold(&hold).unwrap();
    let mut row = db(&f).resume_intent(&run.to_string()).unwrap().unwrap();
    let saved = row.clone();
    row.incarnation = "different-incarnation".into();
    f._storage
        .handle()
        .save_resume_intent(row, Some(saved.incarnation.clone()))
        .unwrap();
    assert!(evidence.validate_hold(&hold).is_err());
    f._storage
        .handle()
        .save_resume_intent(saved, Some("different-incarnation".into()))
        .unwrap();
    f.cleanup(run).await;
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
    assert!(runtime.adopted_restart_ownership_evidence().is_err());
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
