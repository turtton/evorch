//! Characterize the pre-18a policy through the real host, not a second predicate.
use super::*;
use crate::bootstrap::{BootstrapOptions, TestSeams, core_startup::CoreStartupDiagnostics};
use runtime::{AgentModel, Role, RunConfig, StopScope};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

#[derive(Default)]
struct Model {
    entered: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "turn complete".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "idle-test".into()
    }
}

struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

async fn open(dir: &test_socket_dir::SplitDir, model: Arc<Model>) -> BootstrappedHost {
    BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: dir.path().into(),
            project_root: dir.path().into(),
            ownership_root: dir.sockets().into(),
        },
        TestSeams {
            model,
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn conservative_policy_retains_running_and_ordinary_input_waiting_runs() {
    let dir = test_socket_dir::SplitDir::new();
    let model = Arc::new(Model::default());
    let mut host = open(&dir, model.clone()).await;
    assert!(!host.busy(), "passive bootstrap services are not work");
    let mut events = host.runtime.event_bus().subscribe();
    let run = host.runtime.delegate_background(
        Role::Worker,
        "one turn".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    model.entered.notified().await;
    assert!(host.busy(), "an active model turn retains the process");
    model.release.notify_one();
    loop {
        if matches!(
            events.recv().await.unwrap().kind,
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Waiting,
                ..
            }) if run_id == run.to_string()
        ) {
            break;
        }
    }
    // Deliberately unchanged here; the feature PR will relax ONLY this case.
    assert!(host.busy(), "ordinary input wait still retains before 18a");
    host.runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert_eq!(
        host.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
    assert!(!host.busy(), "a stopped run alone is not work");
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn counted_operations_retain_until_their_completion_notification() {
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir, Arc::new(Model::default())).await;
    let ended = host.area_work_notify();
    let (finish, finished) = tokio::sync::oneshot::channel::<()>();
    host.area_tasks
        .spawn_counted("idle-test-operation", |_| async move {
            finished.await?;
            Ok(())
        })
        .unwrap();
    assert!(host.busy(), "counted work retains even without a run");
    finish.send(()).unwrap();
    ended.notified().await;
    assert!(!host.busy());
    host.shutdown().await.unwrap();
}

#[derive(Default)]
struct RecoveryRetry(AtomicBool);
impl CoreStartupDiagnostics for RecoveryRetry {
    fn snapshot(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    fn has_pending_work(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn recovery_retry_and_restart_gate_retain_without_registered_runs() {
    use crate::bootstrap::restart::HostRestartPhase;
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir, Arc::new(Model::default())).await;
    let retry = Arc::new(RecoveryRetry::default());
    host.recovery_diagnostics = Some(retry.clone());
    assert!(!host.busy());
    retry.0.store(true, Ordering::SeqCst);
    assert!(host.busy());
    retry.0.store(false, Ordering::SeqCst);
    assert!(!host.busy());
    let gate = host.host.lock().unwrap().service().restart_gate();
    assert!(gate.transition(1, HostRestartPhase::Preparing));
    assert!(host.busy());
    assert!(gate.transition(1, HostRestartPhase::Open));
    assert!(!host.busy());
    host.shutdown().await.unwrap();
}
