//! Exercise process retention through live runtime boundaries and authorities.
use super::*;
use crate::bootstrap::{BootstrapOptions, TestSeams, core_startup::CoreStartupDiagnostics};
use event_bus::AgentRunPhase;
use runtime::{AgentModel, Role, RunConfig, StopScope};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

#[derive(Default)]
struct Model {
    entered: Notify,
    release: Notify,
    script: Mutex<std::collections::HashMap<String, providers::ContentBlock>>,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.entered.notify_one();
        self.release.notified().await;
        let scripted = self.script.lock().unwrap().remove(&invocation.run_id);
        let tool = scripted.is_some();
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![scripted.unwrap_or(providers::ContentBlock::Text {
                    text: "turn complete".into(),
                })],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: if tool {
                providers::FinishReason::ToolUse
            } else {
                providers::FinishReason::Stop
            },
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
async fn active_turns_retain_but_completed_chat_input_waits_do_not() {
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
    assert!(host.runtime.is_between_chat_turns(run));
    assert!(
        !host.busy(),
        "a completed chat waiting for its next input is idle"
    );
    host.runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert_eq!(
        host.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
    assert!(!host.busy(), "a stopped run alone is not work");
    host.shutdown().await.unwrap();
}

async fn waiting(
    runtime: &runtime::AgentRuntime,
    run: runtime::RunId,
    events: &mut event_bus::EventReceiver,
) {
    loop {
        if matches!(
            events.recv().await.unwrap().kind,
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id, to: AgentRunPhase::Waiting, ..
            }) if run_id == run.to_string()
        ) {
            assert_eq!(
                runtime
                    .list_agents()
                    .into_iter()
                    .find(|agent| agent.run_id == run)
                    .unwrap()
                    .phase,
                AgentRunPhase::Waiting
            );
            return;
        }
    }
}

#[tokio::test]
async fn unanswered_questions_retain_even_after_the_requester_stops() {
    for blocking in [false, true] {
        let dir = test_socket_dir::SplitDir::new();
        let model = Arc::new(Model::default());
        let mut host = open(&dir, model.clone()).await;
        let mut events = host.runtime.event_bus().subscribe();
        let run = host.runtime.delegate_background(
            Role::Worker,
            "chat".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        model.entered.notified().await;
        model.release.notify_one();
        waiting(&host.runtime, run, &mut events).await;
        assert!(!host.busy());
        let question = host
            .runtime
            .request_user_question(run, "retain until answered".into(), vec![], blocking)
            .unwrap();
        assert!(
            host.busy(),
            "pending questions are not ordinary input waits"
        );
        host.runtime.stop(run, StopScope::SelfOnly).unwrap();
        host.runtime.wait(run).await.unwrap();
        assert!(
            host.busy(),
            "unloaded/terminal requesters do not make a question recoverable"
        );
        host.runtime
            .answer_user_question(&question.id, "answer")
            .unwrap();
        assert!(!host.busy(), "an answered question alone does not retain");
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn approval_authority_retains_without_any_live_run() {
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir, Arc::new(Model::default())).await;
    tokio::time::pause();
    let registry = sandbox::approval::ApprovalRegistry::for_bus(&host.runtime.event_bus());
    let pending = registry.register(
        sandbox::approval::ApprovalRequest {
            run_id: "not-loaded",
            thread_id: None,
            tool_name: "shell",
            call_id: "needs-confirmation",
            input: None,
        },
        std::time::Duration::from_secs(60),
    );
    assert!(host.busy());
    drop(pending);
    assert!(
        !host.busy(),
        "cancelled approval authority no longer retains"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_live_child_wait_is_not_an_idle_chat_boundary() {
    let dir = test_socket_dir::SplitDir::new();
    let model = Arc::new(Model::default());
    let mut host = open(&dir, model.clone()).await;
    let mut events = host.runtime.event_bus().subscribe();
    let parent = host.runtime.delegate_background(
        Role::Orchestrator,
        "wait for child".into(),
        RunConfig::default(),
    );
    model.entered.notified().await;
    let child = host
        .runtime
        .delegate_background_as_child(parent, Role::Worker, "working", RunConfig::default())
        .unwrap();
    model.entered.notified().await;
    model.script.lock().unwrap().insert(
        parent.to_string(),
        providers::ContentBlock::ToolUse {
            id: "active-wait".into(),
            name: "wait".into(),
            input: serde_json::json!({"run_id": child.to_string()}),
        },
    );
    model.release.notify_one();
    waiting(&host.runtime, parent, &mut events).await;
    assert!(!host.runtime.is_between_chat_turns(parent));
    assert!(host.busy());
    host.runtime.stop(parent, StopScope::Subtree).unwrap();
    host.runtime.wait(parent).await.unwrap();
    host.runtime.wait(child).await.unwrap();
    assert!(!host.busy());
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

#[tokio::test]
async fn a_cli_invocation_retains_until_it_is_released_and_only_then() {
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir, Arc::new(Model::default())).await;
    let gate = host.host.lock().unwrap().service().restart_gate();
    assert!(!host.busy());
    let first = gate.enter_invocation().unwrap();
    let second = gate.enter_invocation().unwrap();
    assert!(host.busy(), "a running invocation retains the process");
    drop(first);
    assert!(host.busy(), "one released, one still running");
    drop(second);
    assert!(!host.busy(), "nothing left: the idle grace may start");
    host.shutdown().await.unwrap();
}
