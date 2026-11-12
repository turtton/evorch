use super::tests::{pause, terminal};
use super::*;
use crate::{AgentInvocationContext, AgentModel};
use std::sync::mpsc;

fn memo(source: RunId) -> crate::EscalationMemo {
    crate::EscalationMemo {
        source_run_id: source,
        original_request: "original".into(),
        findings: vec![],
        files_touched: vec![],
        blockers: vec![],
        workspace_state: String::new(),
        escalation_reason: "coordinate".into(),
        suggested_next: "continue".into(),
    }
}

struct Model {
    reject: AtomicBool,
    paused_worker: Mutex<Option<WorkerCompletion>>,
    reviewer_entered: Mutex<Option<mpsc::Sender<()>>>,
}

struct WorkerCompletion {
    entered: mpsc::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac15_handoff_failure_completes_only_reserved_execution_and_allows_retry() {
    let (runtime, _model, _storage, _dir) = fixture();
    assert!(runtime.shared.run_ids.use_sequential());
    let source = terminal(&runtime, None, Role::Worker).await;
    let target = RunId::new(2);
    let execution = runtime.reserve_admission(target, None, None);
    let expected = runtime.execution_snapshot(target).unwrap();
    assert!(Arc::ptr_eq(&execution, &expected));
    let thread = event_bus::escalation_thread_id(&target.to_string());
    runtime.bind_thread_root(&thread, target).unwrap();
    let publication = ExecutionControl::new(false).publication();
    let result = runtime.prepare_handoff_to_lane(
        &memo(source),
        &RunConfig::default(),
        Default::default(),
        &publication,
    );
    assert!(result.unwrap_err().contains("already exists"));
    assert!(runtime.wait_admission(target).await.is_err());
    assert!(!lock_runs(&runtime.shared.runs).contains_key(&target));
    // Prepared is deliberately not a resumable target. Retry the source's
    // operation after removing the conflicting goal binding, keeping its ID.
    assert!(
        runtime
            .continue_goal(target, "retry".into(), RunConfig::default())
            .is_err()
    );
    runtime.goal_lock().roots.remove(&target);
    let mut retry_memo = memo(source);
    retry_memo.original_request = "same operation, reworded request".into();
    let retry = runtime
        .prepare_handoff_to_lane(
            &retry_memo,
            &RunConfig::default(),
            Default::default(),
            &publication,
        )
        .unwrap();
    assert_eq!(retry, target);
    let b = runtime.execution_snapshot(retry).unwrap();
    assert!(!Arc::ptr_eq(&execution, &b));
    runtime.fail_reserved_admission(target, &execution, "late old failure".into());
    runtime.update_admission_owner(target, &execution, None);
    b.publication().commit(|| Ok(())).unwrap();
    runtime.cancel(target).unwrap();
}

#[async_trait::async_trait]
impl AgentModel for Model {
    fn requires_admission(&self) -> bool {
        self.reject.load(Ordering::SeqCst)
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        Err(RuntimeError::Model {
            reason: "old admission failure".into(),
        })
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        role: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        if role == Role::Reviewer {
            if let Some(entered) = self.reviewer_entered.lock().unwrap().take() {
                entered.send(()).unwrap();
            }
            std::future::pending::<()>().await;
        }
        let paused = self.paused_worker.lock().unwrap().take();
        if let Some(WorkerCompletion { entered, release }) = paused {
            entered.send(()).unwrap();
            let _ = release.await;
        }
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: Default::default(),
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "goals".into()
    }
}

fn fixture() -> (
    AgentRuntime,
    Arc<Model>,
    storage::Storage,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("goals.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(128));
    let model = Arc::new(Model {
        reject: AtomicBool::new(false),
        paused_worker: Mutex::new(None),
        reviewer_entered: Mutex::new(None),
    });
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    (runtime, model, storage, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac15_old_admission_failure_preserves_new_goal_before_and_after_execution_registration() {
    for before_registration in [false, true] {
        let (runtime, model, _storage, _dir) = fixture();
        let root = terminal(&runtime, None, Role::Worker).await;
        runtime.bind_thread_root("goal", root).unwrap();
        runtime
            .create_thread_goal("goal", root, "original".into(), vec!["criterion".into()])
            .unwrap();
        let a = runtime.execution_snapshot(root).unwrap();
        let (worker_entered_tx, worker_entered) = mpsc::channel();
        let (release_worker, worker_release) = tokio::sync::oneshot::channel();
        *model.paused_worker.lock().unwrap() = Some(WorkerCompletion {
            entered: worker_entered_tx,
            release: worker_release,
        });
        let (reviewer_entered_tx, reviewer_entered) = mpsc::channel();
        *model.reviewer_entered.lock().unwrap() = Some(reviewer_entered_tx);
        let (failed, release_failure) = pause(&runtime, HookPoint::AdmissionFailed, &a);
        let (validated, release_validation) = pause(&runtime, HookPoint::RootRestoreValidated, &a);
        let (completed_tx, completed) = std::sync::mpsc::channel();
        runtime.on_execution(HookPoint::AdmissionCompleted, &a, move || {
            completed_tx.send(()).unwrap()
        });
        let (updated, release_resume) = pause(&runtime, HookPoint::RootGoalUpdated, &a);
        let handle = tokio::runtime::Handle::current();
        std::thread::scope(|scope| {
            let release_resume = release_resume;
            let mut resume = Some(scope.spawn(|| {
                let _enter = handle.enter();
                runtime
                    .continue_goal(root, "new input".into(), RunConfig::default())
                    .unwrap()
            }));
            validated.recv().unwrap();
            model.reject.store(true, Ordering::SeqCst);
            {
                let intents = runtime.shared.spawn_intents.lock().unwrap();
                runtime
                    .admit_run(
                        root,
                        None,
                        Role::Worker,
                        "handoff".into(),
                        RunConfig::default(),
                        RunContinuation::Handoff(RunHandoff {
                            continuity: Default::default(),
                            pending_reference_id: None,
                            source_run_id: root,
                            restored: None,
                            seed_revision: 0,
                        }),
                        None,
                        Arc::clone(&a),
                        &intents,
                    )
                    .unwrap();
            }
            failed.recv().unwrap();
            model.reject.store(false, Ordering::SeqCst);
            release_validation.send(()).unwrap();
            updated.recv().unwrap();
            if !before_registration {
                release_resume.send(()).unwrap();
                resume.take().unwrap().join().unwrap();
                worker_entered.recv().unwrap();
            }
            let reviewer = runtime.reserve_run_id();
            runtime.spawn_goal_reviewer(root, reviewer, "review".into(), RunConfig::default());
            runtime.set_test_goal_reviewer(root, reviewer);
            let reviewer_execution = runtime.execution_snapshot(reviewer).unwrap();
            reviewer_entered.recv().unwrap();
            // Requests record goal usage before complete. Observe both entries
            // and park the worker response so only the stale admission failure
            // can change the goal between the snapshots below.
            let before = runtime.goal_for_root(root).unwrap();
            assert_eq!(
                before.usage.model_requests,
                if before_registration { 1 } else { 2 }
            );
            assert!(!before.work_stopped);
            release_failure.send(()).unwrap();
            // The provider completion has finished its goal side effects before
            // this one-shot notification (no timing-based ordering).
            completed.recv().unwrap();
            assert_eq!(runtime.goal_for_root(root).unwrap(), before);
            assert_eq!(runtime.test_goal_reviewer(root), Some(reviewer));
            assert_eq!(*reviewer_execution.cancel_tx.borrow(), RunInterrupt::None);
            release_worker.send(()).unwrap();
            if before_registration {
                release_resume.send(()).unwrap();
                resume.take().unwrap().join().unwrap();
                worker_entered.recv().unwrap();
            }
        });
        runtime.cancel_subtree(root).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac15_handoff_reserves_before_goal_lock_while_old_failure_finishes() {
    let (runtime, model, _storage, _dir) = fixture();
    let root = terminal(&runtime, None, Role::Worker).await;
    runtime.bind_thread_root("goal", root).unwrap();
    runtime
        .create_thread_goal("goal", root, "original".into(), vec!["criterion".into()])
        .unwrap();
    let a = runtime.execution_snapshot(root).unwrap();
    let (checkpoint, release_failure) = pause(&runtime, HookPoint::AdmissionGoalCheckpoint, &a);
    model.reject.store(true, Ordering::SeqCst);
    {
        let intents = runtime.shared.spawn_intents.lock().unwrap();
        runtime
            .admit_run(
                root,
                None,
                Role::Worker,
                "old".into(),
                RunConfig::default(),
                RunContinuation::Handoff(RunHandoff {
                    continuity: Default::default(),
                    pending_reference_id: None,
                    source_run_id: root,
                    restored: None,
                    seed_revision: 0,
                }),
                None,
                Arc::clone(&a),
                &intents,
            )
            .unwrap();
    }
    checkpoint.recv().unwrap();
    model.reject.store(false, Ordering::SeqCst);
    let (reserving_tx, reserving) = std::sync::mpsc::channel();
    runtime.on_execution(HookPoint::HandoffBeforeReserve, &a, move || {
        reserving_tx.send(()).unwrap()
    });
    let handle = tokio::runtime::Handle::current();
    std::thread::scope(|scope| {
        let release_failure = release_failure;
        let runtime = &runtime;
        let handoff = scope.spawn(move || {
            let _enter = handle.enter();
            runtime.spawn_escalated_root(
                memo(root),
                &RunConfig::default(),
                Default::default(),
                || {},
            )
        });
        reserving.recv().unwrap();
        assert!(
            runtime.shared.thread_goals.try_lock().is_ok(),
            "handoff must not hold Q while reserving I"
        );
        release_failure.send(()).unwrap();
        let target = handoff.join().unwrap().unwrap();
        assert_eq!(runtime.goal_thread(root), None);
        assert_eq!(
            runtime.goal_thread(target),
            Some(event_bus::escalation_thread_id(&target.to_string()))
        );
        runtime.cancel(target).unwrap();
    });
}
