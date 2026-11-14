use super::{tests::pause, *};
use crate::ownership::OwnerHost;
use event_bus::{LaneBrief, LanePolicy};
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Model {
    calls: AtomicUsize,
    hold_admission: AtomicBool,
    admission_entered: tokio::sync::Notify,
    release_admission: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl crate::AgentModel for Model {
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &crate::AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        if self.hold_admission.load(Ordering::SeqCst) {
            self.admission_entered.notify_one();
            self.release_admission.notified().await;
        }
        Ok(())
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

struct Fixture {
    runtime: AgentRuntime,
    model: Arc<Model>,
    storage: storage::Storage,
    owner: OwnerHost,
    run: RunId,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("lanes.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let owner =
            OwnerHost::open(&dir.path().join("owners"), Default::default(), bus.clone()).unwrap();
        let model = Arc::new(Model::default());
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime.reserve_run_id();
        storage
            .handle()
            .create_lane(storage::lanes::NewLane {
                lane_id: "lane".into(),
                thread_id: "lane-thread".into(),
                origin_thread_id: "lead-thread".into(),
                origin_run_id: "lead".into(),
                creation_key: "create".into(),
                brief: LaneBrief {
                    objective: "a fenced lane".into(),
                    inputs: vec![],
                    deliverables: vec!["artifact".into()],
                    acceptance_criteria: vec!["review".into()],
                    constraints: vec![],
                    verification_requirements: Default::default(),
                    workspace: Default::default(),
                    report_policy: LanePolicy::Attached,
                },
                start_seed: serde_json::json!({
                    "run_id": run, "project_root": null, "requires_ownership": true,
                })
                .to_string(),
                project_id: None,
                project_root: None,
                title: None,
                at_ns: 1,
            })
            .unwrap();
        Self {
            runtime,
            model,
            storage,
            owner,
            run,
            dir,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_after_lane_preflight_cannot_write_initial_context_or_failure_report() {
    let f = Fixture::new();
    let permit = f.owner.start("lane-thread").unwrap();
    let execution = f
        .runtime
        .reserve_admission(f.run, None, Some(permit.clone()));
    let (reached, release) = pause(&f.runtime, HookPoint::LaneOwnershipValidated, &execution);
    assert_eq!(
        f.runtime.recover_unstarted_lanes(Some(&f.owner)).unwrap(),
        vec![f.run]
    );
    reached.recv().unwrap();
    let next = OwnerHost::open(
        &f.dir.path().join("owners"),
        Default::default(),
        Arc::new(EventBus::new(128)),
    )
    .unwrap();
    f.owner.handoff(&permit, &next).unwrap();
    release.send(()).unwrap();
    assert_eq!(f.runtime.wait(f.run).await.unwrap(), AgentRunPhase::Error);
    // Registration already succeeded; do not retroactively change that result.
    assert!(matches!(
        f.runtime.admission_snapshot(f.run).unwrap().result,
        Some(Ok(()))
    ));
    assert_eq!(
        lock_runs(&f.runtime.shared.runs)[&f.run]
            .terminal_reason
            .as_deref(),
        Some(
            RuntimeError::StaleOwnership {
                run_id: f.run.to_string()
            }
            .to_string()
            .as_str()
        )
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert!(
        f.runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(f.run)
            .unwrap()
            .is_none()
    );
    let lane = f.storage.handle().lane("lane").unwrap().unwrap();
    assert!(!lane.is_started());
    assert_eq!(lane.latest_report_seq, 0);
    assert!(
        f.storage
            .handle()
            .lane_reports("lane", 0, 10)
            .unwrap()
            .is_empty()
    );
}

fn pause_clock(
    clock: &Arc<crate::ownership::VirtualClock>,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered, reached) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let mut once = Some((entered, resume));
    clock.on_sleep(move |_| {
        if let Some((entered, resume)) = once.take() {
            entered.send(()).unwrap();
            let _ = resume.recv();
        }
    });
    (reached, release)
}

struct Contention {
    lock: rusqlite::Connection,
    permit: crate::ownership::OwnerPermit,
    clock: Arc<crate::ownership::VirtualClock>,
    _clock: crate::ownership::GenerationClockOverride,
}

impl Fixture {
    async fn interrupt_contended_admission(
        &self,
        signal: RunInterrupt,
        subtree: bool,
    ) -> Contention {
        let permit = self.owner.start("lane-thread").unwrap();
        self.model.hold_admission.store(true, Ordering::SeqCst);
        assert_eq!(
            self.runtime
                .recover_unstarted_lanes(Some(&self.owner))
                .unwrap(),
            vec![self.run]
        );
        self.model.admission_entered.notified().await;
        let clock = crate::ownership::VirtualClock::new();
        let override_clock = permit.set_generation_clock_for_test(&clock);
        let lock = rusqlite::Connection::open(&permit.registry_path).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let (busy, release) = pause_clock(&clock);
        self.model.release_admission.notify_one();
        busy.recv().unwrap();
        let result = match signal {
            RunInterrupt::Stop => self.runtime.stop(self.run, StopScope::SelfOnly),
            RunInterrupt::Cancel if subtree => self.runtime.cancel_subtree(self.run),
            RunInterrupt::Cancel => self.runtime.cancel(self.run),
            _ => unreachable!(),
        };
        let execution = self.runtime.execution_snapshot(self.run).unwrap();
        let notified = *execution.cancel_tx.borrow();
        let fenced = execution.cancelled();
        if notified != signal {
            // A regression must not leave an unbounded blocking-pool retry
            // alive while the failing test's Tokio runtime is being dropped.
            let _ = self.runtime.cancel(self.run);
        }
        release.send(()).unwrap();
        let settled = self.runtime.wait(self.run).await;
        assert!(
            matches!(result, Err(RuntimeError::LaneCancellationPending { .. })),
            "{result:?}"
        );
        assert_eq!(notified, signal);
        assert!(fenced);
        assert!(matches!(settled, Err(RuntimeError::RunTerminated { .. })));
        // Still EXCLUSIVE: settlement must not depend on unlocking the registry.
        assert!(!lock.is_autocommit());
        assert!(self.runtime.list_agents().is_empty());
        assert_eq!(self.model.calls.load(Ordering::SeqCst), 0);
        assert_eq!(self.runtime.pending_lane_stop_writes(), 1);
        assert!(
            self.runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .restore_record(self.run)
                .unwrap()
                .is_none()
        );
        Contention {
            lock,
            permit,
            clock,
            _clock: override_clock,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_lane_interrupt_settles_while_exclusive_then_drains_without_global_locks() {
    for (signal, subtree) in [
        (RunInterrupt::Stop, false),
        (RunInterrupt::Cancel, false),
        (RunInterrupt::Cancel, true),
    ] {
        let f = Fixture::new();
        let contention = f.interrupt_contended_admission(signal, subtree).await;
        let (busy, release) = pause_clock(&contention.clock);
        let runtime = f.runtime.clone();
        let drain = tokio::spawn(async move { runtime.drain_lane_stop_writes().await });
        busy.recv().unwrap();
        assert!(f.runtime.shared.spawn_intents.try_lock().is_ok());
        assert!(f.runtime.shared.admissions.try_lock().is_ok());
        assert!(f.runtime.shared.runs.try_lock().is_ok());
        // A distinct admission can make progress while durable Stop retries.
        let other = f.runtime.reserve_run_id();
        f.runtime.reserve_admission(other, None, None);
        f.runtime.cancel(other).unwrap();
        contention.lock.execute_batch("ROLLBACK").unwrap();
        release.send(()).unwrap();
        drain.await.unwrap().unwrap();
        assert_eq!(f.runtime.pending_lane_stop_writes(), 0);
        let lane = f.storage.handle().lane("lane").unwrap().unwrap();
        let reason = match signal {
            RunInterrupt::Stop => storage::lanes::LANE_START_OPERATOR_STOP,
            _ => "cancelled before root start",
        };
        assert_eq!(lane.start_blocked_reason.as_deref(), Some(reason));
        f.runtime.drain_lane_stop_writes().await.unwrap();
        assert_eq!(
            f.storage
                .handle()
                .lane("lane")
                .unwrap()
                .unwrap()
                .updated_at_ns,
            lane.updated_at_ns
        );
        let bus = Arc::new(EventBus::new(128));
        let config = storage::StorageConfig {
            db_path: f.dir.path().join("lanes.db"),
            ..Default::default()
        };
        let restarted = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            f.model.clone(),
        )
        .with_run_store(crate::RunStore::open(&config, f.storage.handle()).unwrap());
        assert!(
            restarted
                .recover_unstarted_lanes(Some(&f.owner))
                .unwrap()
                .is_empty()
        );
        assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_hardening_during_writeback_is_not_erased_or_left_after_drain() {
    let f = Fixture::new();
    let contention = f
        .interrupt_contended_admission(RunInterrupt::Stop, false)
        .await;
    contention.lock.execute_batch("ROLLBACK").unwrap();
    // The virtual clock only coordinates the deliberately blocked admission.
    // Heartbeat writes run on a real OS thread; advancing a whole retry budget
    // instantly can exhaust the drain before that writer releases SQLite.
    drop(contention._clock);
    let execution = f.runtime.execution_snapshot(f.run).unwrap();
    // Stop this exact writeback after capturing the immutable Stop request,
    // rather than an arbitrary reader that sleeps on the shared clock.
    let (writing, release) = pause(&f.runtime, HookPoint::LaneStopWriteStarted, &execution);
    let runtime = f.runtime.clone();
    let drain = tokio::spawn(async move { runtime.drain_lane_stop_writes().await });
    writing.recv().unwrap();
    assert!(matches!(
        f.runtime.cancel(f.run),
        Err(RuntimeError::LaneCancellationPending { .. })
    ));
    release.send(()).unwrap();
    drain.await.unwrap().unwrap();
    assert_eq!(f.runtime.pending_lane_stop_writes(), 0);
    assert_eq!(
        f.storage
            .handle()
            .lane("lane")
            .unwrap()
            .unwrap()
            .start_blocked_reason
            .as_deref(),
        Some("cancelled before root start")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_lane_stop_cannot_write_after_handoff_and_reports_the_unrecorded_stop() {
    let f = Fixture::new();
    let contention = f
        .interrupt_contended_admission(RunInterrupt::Stop, false)
        .await;
    contention.lock.execute_batch("ROLLBACK").unwrap();
    let next = OwnerHost::open(
        &f.dir.path().join("owners"),
        Default::default(),
        Arc::new(EventBus::new(128)),
    )
    .unwrap();
    f.owner.handoff(&contention.permit, &next).unwrap();
    assert!(matches!(
        f.runtime.cancel(f.run),
        Err(RuntimeError::LaneCancellationPending { .. })
    ));
    let error = f.runtime.drain_lane_stop_writes().await.unwrap_err();
    assert!(matches!(error, RuntimeError::LaneCancellation { .. }));
    assert!(
        error
            .to_string()
            .contains("durable cancellation was not recorded")
    );
    assert_eq!(f.runtime.pending_lane_stop_writes(), 1);
    assert!(
        f.storage
            .handle()
            .lane("lane")
            .unwrap()
            .unwrap()
            .start_blocked_reason
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settled_unregistered_lane_keeps_its_permit_without_an_existing_stop_queue() {
    let f = Fixture::new();
    let contention = f
        .interrupt_contended_admission(RunInterrupt::Stop, false)
        .await;
    contention.lock.execute_batch("ROLLBACK").unwrap();
    f.runtime.drain_lane_stop_writes().await.unwrap();
    let next = OwnerHost::open(
        &f.dir.path().join("owners"),
        Default::default(),
        Arc::new(EventBus::new(128)),
    )
    .unwrap();
    f.owner.handoff(&contention.permit, &next).unwrap();
    assert!(matches!(
        f.runtime.cancel(f.run),
        Err(RuntimeError::LaneCancellation { .. })
    ));
    assert!(f.runtime.drain_lane_stop_writes().await.is_err());
    assert_eq!(f.runtime.pending_lane_stop_writes(), 1);
    assert_eq!(
        f.storage
            .handle()
            .lane("lane")
            .unwrap()
            .unwrap()
            .start_blocked_reason
            .as_deref(),
        Some(storage::lanes::LANE_START_OPERATOR_STOP)
    );
}

#[tokio::test(start_paused = true)]
async fn lane_stop_writeback_retries_storage_recovery_and_stop_interrupts_its_backoff() {
    for recover in [true, false] {
        let f = Fixture::new();
        f.model.hold_admission.store(true, Ordering::SeqCst);
        f.runtime.recover_unstarted_lanes(Some(&f.owner)).unwrap();
        f.model.admission_entered.notified().await;
        let lock = rusqlite::Connection::open(f.dir.path().join("lanes.db")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let stopped = f.runtime.stop(f.run, StopScope::SelfOnly);
        assert!(
            matches!(stopped, Err(RuntimeError::LaneCancellation { .. })),
            "unexpected Stop result: {stopped:?}"
        );
        assert!(f.runtime.wait(f.run).await.is_err());
        assert_eq!(f.runtime.pending_lane_stop_writes(), 1);
        let execution = f.runtime.execution_snapshot(f.run).unwrap();
        let (failed, failure_observed) = tokio::sync::oneshot::channel();
        f.runtime
            .on_execution(HookPoint::LaneStopWriteFailed, &execution, move || {
                failed.send(()).unwrap();
            });
        let (written, write_observed) = tokio::sync::oneshot::channel();
        f.runtime
            .on_execution(HookPoint::LaneStopPersisted, &execution, move || {
                let _ = written.send(());
            });
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let runtime = f.runtime.clone();
        let service = tokio::spawn(async move { runtime.run_lane_stop_writes(stopped).await });
        failure_observed.await.unwrap();
        if recover {
            lock.execute_batch("ROLLBACK").unwrap();
            // Paused Tokio time advances to the backoff tick. No new runtime
            // notification is sent after the external SQLite writer unlocks.
            write_observed.await.unwrap();
            stop.send(()).unwrap();
            service.await.unwrap().unwrap();
        } else {
            stop.send(()).unwrap();
            assert!(service.await.unwrap().is_err());
            assert_eq!(f.runtime.pending_lane_stop_writes(), 1);
            lock.execute_batch("ROLLBACK").unwrap();
            f.runtime.drain_lane_stop_writes().await.unwrap();
        }
        assert_eq!(f.runtime.pending_lane_stop_writes(), 0);
        assert_eq!(
            f.storage
                .handle()
                .lane("lane")
                .unwrap()
                .unwrap()
                .start_blocked_reason
                .as_deref(),
            Some(storage::lanes::LANE_START_OPERATOR_STOP)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_stop_service_shutdown_has_a_bounded_drain_and_retains_failed_writeback() {
    let f = Fixture::new();
    let contention = f
        .interrupt_contended_admission(RunInterrupt::Stop, false)
        .await;
    let (busy, release) = pause_clock(&contention.clock);
    let (stop, receiver) = tokio::sync::oneshot::channel();
    let runtime = f.runtime.clone();
    let service = tokio::spawn(async move { runtime.run_lane_stop_writes(receiver).await });
    busy.recv().unwrap();
    stop.send(()).unwrap();
    release.send(()).unwrap();
    // Only the ownership budget uses virtual time. No completion timeout or
    // machine-speed bound is part of the test's pass condition.
    let error = service.await.unwrap().unwrap_err();
    assert!(matches!(error, RuntimeError::LaneCancellation { .. }));
    assert_eq!(Arc::strong_count(&contention.clock), 1);
    assert_eq!(f.runtime.pending_lane_stop_writes(), 1);
    assert!(!contention.lock.is_autocommit());
    assert!(
        f.storage
            .handle()
            .lane("lane")
            .unwrap()
            .unwrap()
            .start_blocked_reason
            .is_none()
    );
    contention.lock.execute_batch("ROLLBACK").unwrap();
    f.runtime.drain_lane_stop_writes().await.unwrap();
    assert_eq!(f.runtime.pending_lane_stop_writes(), 0);
}
