#![cfg(target_os = "linux")]
//! Uses the real per-run tool composition, not a Shell injected into the base executor.
#[path = "../shared-test-support/restart_model.rs"]
mod input;
use runtime::AgentModel;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};

#[derive(Default)]
struct Worker {
    input: input::Model,
    issued: AtomicBool,
    entered: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AgentModel for Worker {
    fn selected_model(&self, role: runtime::Role, hint: Option<&str>) -> String {
        self.input.selected_model(role, hint)
    }
    fn requires_admission(&self) -> bool {
        false
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
    ) -> Result<(), runtime::RuntimeError> {
        Ok(())
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: runtime::Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        let mut response = self
            .input
            .complete(invocation, role, messages, tools)
            .await?;
        if !self.issued.swap(true, Ordering::SeqCst) {
            response.message.content = vec![providers::ContentBlock::ToolUse {
                id: "real-shell".into(),
                name: "shell".into(),
                input: serde_json::json!({"command":"read value", "yield_ms":0}),
            }];
            response.finish_reason = providers::FinishReason::ToolUse;
            return Ok(response);
        }
        assert!(messages.last().unwrap().content.iter().any(|block|
            matches!(block, providers::ContentBlock::ToolResult { tool_call_id, is_error:false, .. } if tool_call_id == "real-shell")));
        self.entered.notify_one();
        // The provider stays in flight until the real host's Stop cancels it.
        std::future::pending().await
    }
}

#[tokio::test]
async fn shell_blocker_listing_finds_the_real_worker_executor() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let model = Arc::new(Worker::default());
    let mut host = BootstrappedHost::open_for_core_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: root.join("o"),
        },
        TestSeams {
            model: model.clone(),
            sandbox: Arc::new(input::Direct),
        },
        &evorch_core::recovery_service::CoreGraphRecovery,
    )
    .await
    .unwrap();
    let run = host
        .runtime
        .delegate_chat(
            "thread",
            runtime::Role::Worker,
            "shell".into(),
            runtime::RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    model.entered.notified().await;
    let attempt = host.runtime.begin_restart_quiesce().unwrap();
    let jobs = host
        .runtime
        .restart_shell_jobs(&attempt, "restart", "epoch")
        .unwrap();
    host.runtime.cancel_restart_quiesce(&attempt).unwrap();
    host.shutdown().await.unwrap();
    assert_eq!(
        jobs.len(),
        1,
        "the real Worker's shell must appear in ShellBlockers"
    );
    assert_eq!(jobs[0].run_id(), run);
    assert_eq!(jobs[0].snapshot().state, tools::ShellJobState::Running);
}
