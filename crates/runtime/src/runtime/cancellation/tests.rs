use super::*;
use crate::{AgentInvocationContext, AgentModel};
use std::sync::mpsc;

struct CompletingModel;

#[async_trait::async_trait]
impl AgentModel for CompletingModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        Ok(providers::ChatResponse {
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
        "test".into()
    }
}

fn fixture() -> (AgentRuntime, storage::Storage, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("runs.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(CompletingModel),
    )
    .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    (runtime, storage, dir)
}

pub(super) async fn terminal(runtime: &AgentRuntime, parent: Option<RunId>, role: Role) -> RunId {
    let run = runtime.reserve_run_id();
    runtime.spawn_reserved(run, parent, role, "material", RunConfig::default());
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    run
}

pub(super) struct Release(mpsc::Sender<()>);
impl Release {
    pub(super) fn send(&self, value: ()) -> Result<(), mpsc::SendError<()>> {
        self.0.send(value)
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

pub(super) fn pause(
    runtime: &AgentRuntime,
    point: HookPoint,
    execution: &Arc<ExecutionControl>,
) -> (mpsc::Receiver<()>, Release) {
    let (entered, reached) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    runtime.on_execution(point, execution, move || {
        entered.send(()).unwrap();
        let _ = resume.recv();
    });
    (reached, Release(release))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac15_cancel_retries_current_execution_after_real_root_resume_before_publish() {
    for overwrite in [false, true] {
        let (runtime, _storage, _dir) = fixture();
        let run = terminal(&runtime, None, Role::Worker).await;
        let a = runtime.execution_snapshot(run).unwrap();
        let (reached, release) = pause(&runtime, HookPoint::CancelGate, &a);
        let workspace = tempfile::tempdir().unwrap();
        let roots = crate::artifacts::SourceRoots::new(workspace.path(), None).unwrap();
        let (path, directory) = roots.destination(std::path::Path::new("hero.png")).unwrap();
        let variant = path.with_file_name("hero.64.png");
        if overwrite {
            std::fs::write(&path, b"old original").unwrap();
            std::fs::write(&variant, b"old variant").unwrap();
        }
        std::thread::scope(|scope| {
            let release = release;
            let cancel = scope.spawn(|| runtime.cancel(run).unwrap());
            reached.recv().unwrap();
            assert_eq!(
                runtime
                    .continue_goal(run, "new request".into(), RunConfig::default())
                    .unwrap(),
                run
            );
            let b = runtime.execution_snapshot(run).unwrap();
            assert!(!Arc::ptr_eq(&a, &b));
            let token = b.publication();
            let receiver = b.cancel_tx.subscribe();
            let saved = crate::image_generation::save_generated(
                &token,
                &directory,
                (&path, b"new original"),
                Some((&variant, b"new variant")),
                || {
                    // The same last interruption check used by image dispatch,
                    // after sync, before either rename.
                    assert!(receiver.borrow().kind().is_none());
                    assert_eq!(
                        std::fs::read_dir(workspace.path()).unwrap().count(),
                        if overwrite { 4 } else { 2 }
                    );
                    release.send(()).unwrap();
                    cancel.join().unwrap();
                    assert_eq!(*receiver.borrow(), RunInterrupt::Cancel);
                    Ok(())
                },
            );
            assert!(saved.unwrap_err().contains("interrupted"));
            assert!(b.cancelled());
            assert!(
                !a.cancelled(),
                "cancel retried instead of targeting the obsolete execution"
            );
        });
        if overwrite {
            assert_eq!(std::fs::read(&path).unwrap(), b"old original");
            assert_eq!(std::fs::read(&variant).unwrap(), b"old variant");
        } else {
            assert!(!path.exists());
            assert!(!variant.exists());
        }
        assert_eq!(
            std::fs::read_dir(workspace.path()).unwrap().count(),
            if overwrite { 2 } else { 0 }
        );
    }
}

#[tokio::test]
async fn ac15_cancelled_old_token_stays_fenced_but_resumed_execution_can_publish() {
    let (runtime, _storage, _dir) = fixture();
    let run = terminal(&runtime, None, Role::Worker).await;
    let a = runtime.execution_snapshot(run).unwrap();
    let old = a.publication();
    runtime.cancel(run).unwrap();
    runtime
        .continue_goal(run, "new request".into(), RunConfig::default())
        .unwrap();
    let b = runtime.execution_snapshot(run).unwrap();
    assert!(!Arc::ptr_eq(&a, &b));
    assert!(
        old.commit(|| -> Result<(), String> { panic!("old execution must never publish") })
            .is_err()
    );
    b.publication().commit(|| Ok(())).unwrap();
    assert_eq!(*b.cancel_tx.borrow(), RunInterrupt::None);
}

struct AdmissionModel;
#[async_trait::async_trait]
impl AgentModel for AdmissionModel {
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "admission".into()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac15_cancel_after_registration_notifies_same_execution() {
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(AdmissionModel),
    );
    let run = runtime.reserve_run_id();
    runtime.reserve_admission(run, None, None);
    let execution = runtime.execution_snapshot(run).unwrap();
    let token = execution.publication();
    let (reached, release) = pause(&runtime, HookPoint::AdmissionRegistered, &execution);
    runtime.spawn_reserved(run, None, Role::Worker, "pending", RunConfig::default());
    reached.recv().unwrap();
    {
        let admissions = runtime.shared.admissions.lock().unwrap();
        let runs = lock_runs(&runtime.shared.runs);
        assert!(matches!(&*admissions[&run].result.borrow(), Some(Ok(()))));
        assert!(Arc::ptr_eq(&admissions[&run].execution, &execution));
        assert!(Arc::ptr_eq(&runs[&run].execution, &execution));
    }
    runtime.cancel(run).unwrap();
    assert_eq!(*execution.cancel_tx.borrow(), RunInterrupt::Cancel);
    assert!(token.commit(|| Ok(())).is_err());
    assert!(matches!(
        &*runtime.shared.admissions.lock().unwrap()[&run]
            .result
            .borrow(),
        Some(Ok(()))
    ));
    release.send(()).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert!(matches!(
        &*runtime.shared.admissions.lock().unwrap()[&run]
            .result
            .borrow(),
        Some(Ok(()))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac15_subtree_collection_cancels_old_execution_not_explicitly_resumed_root() {
    let (runtime, _storage, _dir) = fixture();
    let root = terminal(&runtime, None, Role::Worker).await;
    let child = terminal(&runtime, Some(root), Role::Worker).await;
    let a = runtime.execution_snapshot(root).unwrap();
    let (reached, release) = pause(&runtime, HookPoint::SubtreeCollected, &a);
    std::thread::scope(|scope| {
        let release = release;
        let cancel = scope.spawn(|| runtime.cancel_subtree(root).unwrap());
        reached.recv().unwrap();
        runtime
            .continue_goal(root, "explicit resume".into(), RunConfig::default())
            .unwrap();
        let b = runtime.execution_snapshot(root).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        release.send(()).unwrap();
        cancel.join().unwrap();
        assert!(a.publication().commit(|| Ok(())).is_err());
        b.publication().commit(|| Ok(())).unwrap();
        assert_eq!(*b.cancel_tx.borrow(), RunInterrupt::None);
        assert!(!runtime.spawn_cancelled(root));
        assert!(runtime.spawn_cancelled(child));
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac15_subtree_tombstone_refuses_child_restore_before_cancel_notification() {
    let (runtime, _storage, _dir) = fixture();
    let root = terminal(&runtime, None, Role::Orchestrator).await;
    let child = terminal(&runtime, Some(root), Role::Worker).await;
    let execution = runtime.execution_snapshot(child).unwrap();
    let (reached, release) = pause(&runtime, HookPoint::SubtreeCollected, &execution);
    std::thread::scope(|scope| {
        let release = release;
        let cancel = scope.spawn(|| runtime.cancel_subtree(child).unwrap());
        reached.recv().unwrap();
        assert_eq!(*execution.cancel_tx.borrow(), RunInterrupt::None);
        assert!(matches!(
            runtime.send_agent_message(root, child, AgentMessageKind::Send, "continue", None),
            Err(RuntimeError::RunTerminated { .. })
        ));
        assert!(Arc::ptr_eq(
            &runtime.execution_snapshot(child).unwrap(),
            &execution
        ));
        release.send(()).unwrap();
        cancel.join().unwrap();
    });
}

struct NeverCalledModel {
    admission: bool,
}
#[async_trait::async_trait]
impl AgentModel for NeverCalledModel {
    fn requires_admission(&self) -> bool {
        self.admission
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        panic!("an initially cancelled execution must not call the provider")
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "never-called".into()
    }
}

#[tokio::test]
async fn ac15_inherited_cancel_preserves_spawn_results_and_fences_publication_before_provider() {
    for admission in [false, true] {
        let bus = Arc::new(EventBus::new(64));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(NeverCalledModel { admission }),
        );
        let root = runtime.reserve_run_id();
        runtime.reserve_admission(root, None, None);
        runtime.cancel_subtree(root).unwrap();
        let child = runtime.reserve_run_id();
        runtime.spawn_reserved(
            child,
            Some(root),
            Role::Worker,
            "must not run",
            RunConfig::default(),
        );
        let execution = runtime.execution_snapshot(child).unwrap();
        assert!(execution.cancelled());
        assert_eq!(*execution.cancel_tx.borrow(), RunInterrupt::Cancel);
        assert!(
            execution
                .publication()
                .commit(|| -> Result<(), String> { panic!("cancelled child must not publish") })
                .is_err()
        );
        if admission {
            assert!(matches!(
                runtime.wait(child).await,
                Err(RuntimeError::RunTerminated { .. })
            ));
            assert!(!lock_runs(&runtime.shared.runs).contains_key(&child));
        } else {
            assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Error);
            assert!(Arc::ptr_eq(
                &lock_runs(&runtime.shared.runs)[&child].execution,
                &execution
            ));
        }
    }
}
