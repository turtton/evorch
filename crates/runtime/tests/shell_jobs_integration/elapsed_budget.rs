use super::*;
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};

/// Drive the real shell poll across a virtual budget boundary, releasing its
/// child only after proving that the poll remains pending beyond yield_ms.
struct ClockedShell {
    shell: tools::tools::Shell,
    release: PathBuf,
}

#[async_trait]
impl Tool for ClockedShell {
    fn name(&self) -> &str {
        "shell"
    }
    fn schema(&self) -> serde_json::Value {
        self.shell.schema()
    }
    fn permissions(&self) -> Permissions {
        self.shell.permissions()
    }
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        self.shell.execute(args).await
    }
    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        if args["action"] != "poll" {
            return self.shell.execute_with_context(ctx, args).await;
        }
        let poll = self.shell.execute_with_context(ctx, args);
        tokio::pin!(poll);
        assert!(futures_util::poll!(poll.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(
            futures_util::poll!(poll.as_mut()).is_pending(),
            "yield_ms must not end a poll of a live job"
        );
        let release = self.release.clone();
        tokio::task::spawn_blocking(move || std::fs::write(release, "go\n"))
            .await
            .unwrap()
            .unwrap();
        let result = poll.await?;
        assert_eq!(result.idle_wait_ms, 4_000);
        Ok(result)
    }
    fn cancel_shell_jobs(&self, run: &str) {
        self.shell.cancel_shell_jobs(run);
    }
    async fn drain_shell_jobs(&self, run: &str) -> Result<(), ToolError> {
        self.shell.drain_shell_jobs(run).await
    }
    fn release_shell_jobs(&self, run: &str) -> Result<(), ToolError> {
        self.shell.release_shell_jobs(run)
    }
    fn has_running_shell_jobs(&self, run: &str) -> bool {
        self.shell.has_running_shell_jobs(run)
    }
    fn has_unobserved_shell_jobs(&self, run: &str) -> bool {
        self.shell.has_unobserved_shell_jobs(run)
    }
}

#[tokio::test(start_paused = true)]
async fn a_poll_waits_for_a_long_job_without_spending_the_elapsed_budget() {
    let _clock = paused_process_clock::hold();
    test_support::isolated_state_dir();
    let (_temp, root) = init_git_repo();
    let release = root.join("release");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&release)
            .status()
            .unwrap()
            .success()
    );
    let bus = Arc::new(EventBus::new(256));
    let mut executor = ToolExecutor::with_standard_tools_in(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
        Some(root),
    );
    executor
        .register(Arc::new(ClockedShell {
            shell: tools::tools::Shell::new(Arc::new(sandbox::DirectSandbox::new_unchecked())),
            release,
        }))
        .unwrap();
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model);
    let mut config = RunConfig::default();
    config.budget.max_elapsed =
        runtime::budget_tracker::ResourceLimit::Bounded(Duration::from_secs(2));
    let run = runtime.delegate_background(Role::Worker, "long build".into(), config);
    next(&mut calls).await.respond(tool_response(
        "start",
        "shell",
        json!({"command":"read value < release; printf built", "yield_ms":0}),
    ));
    let call = next(&mut calls).await;
    let job = call.job("start");
    call.respond(tool_response(
        "poll",
        "shell",
        json!({"action":"poll", "job_id":job, "yield_ms":100}),
    ));
    let call = next(&mut calls).await;
    let (result, error) = call.result("poll");
    assert!(!error, "{result}");
    assert_eq!(field(&result, "status: "), "completed");
    assert!(result.contains("built"));
    call.respond(text_response("built", FinishReason::Stop));
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Done);
}
