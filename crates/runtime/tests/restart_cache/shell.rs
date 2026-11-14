use super::*;
use tokio::sync::{Notify, Semaphore};
use tools::{ShellJobIdentity, ShellJobSnapshot};

// Hold the actual shell result at the tool boundary so preparation cannot race
// the next provider request. No fake process/result is used.
struct GatedShell {
    shell: Arc<tools::Shell>,
    started: Notify,
    release: Semaphore,
}
#[async_trait::async_trait]
impl Tool for GatedShell {
    fn name(&self) -> &str {
        "shell"
    }
    fn schema(&self) -> Value {
        self.shell.schema()
    }
    fn permissions(&self) -> tools::Permissions {
        self.shell.permissions()
    }
    async fn execute(&self, _: Value) -> Result<tools::ToolResult, tools::ToolError> {
        unreachable!("context required")
    }
    async fn execute_with_context(
        &self,
        ctx: &tools::ToolExecutionContext,
        args: Value,
    ) -> Result<tools::ToolResult, tools::ToolError> {
        let start = args.get("command").is_some();
        let result = self.shell.execute_with_context(ctx, args).await;
        if start {
            self.started.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
        result
    }
    fn shell_job_snapshots(&self, run: &str) -> Vec<ShellJobSnapshot> {
        self.shell.shell_job_snapshots(run)
    }
    fn subscribe_shell_job_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.shell.subscribe_shell_job_changes()
    }
    async fn wait_shell_job_terminal(
        &self,
        identity: &ShellJobIdentity,
    ) -> Result<ShellJobSnapshot, tools::ToolError> {
        self.shell.wait_shell_job_terminal(identity).await
    }
    fn has_running_shell_jobs(&self, run: &str) -> bool {
        self.shell.has_running_shell_jobs(run)
    }
    fn has_unobserved_shell_jobs(&self, run: &str) -> bool {
        self.shell.has_unobserved_shell_jobs(run)
    }
    fn cancel_shell_jobs(&self, run: &str) {
        self.shell.cancel_shell_jobs(run);
    }
    fn release_shell_jobs(&self, run: &str) -> Result<(), tools::ToolError> {
        self.shell.release_shell_jobs(run)
    }
}

#[tokio::test]
async fn shell_wait_terminal_observation_and_restart_cancel_preserve_wire_prefix() {
    use std::io::Write;
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("release");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let config = storage::StorageConfig {
            db_path: directory.path().join("shell.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let shell = Arc::new(GatedShell {
            shell: Arc::new(tools::Shell::new(Arc::new(
                sandbox::DirectSandbox::new_unchecked(),
            ))),
            started: Notify::new(),
            release: Semaphore::new(0),
        });
        let command = json!({"command":format!("IFS= read -r line < '{}'; printf 'normal:%s' \"$line\"", fifo.display()),"yield_ms":0});
        let mut h = harness_with_wire(
            wire,
            vec![
                read_response(0),
                ScriptedResponse::tool_call(
                    "start-response",
                    MODEL,
                    0,
                    "shell-start",
                    "shell",
                    [command.to_string()],
                ),
            ],
            1_000_000,
            Arc::new(BulkRead),
            vec![shell.clone()],
            None,
            0.9,
            |_| {},
        );
        h.runtime = h
            .runtime
            .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let run = h.runtime.delegate_background(
            Role::Worker,
            "Wait without signalling the shell".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        shell.started.notified().await;
        let attempt = h.runtime.begin_restart_quiesce().unwrap();
        let mut changes = h.runtime.subscribe_restart_state();
        shell.release.add_permits(1);
        while !h
            .runtime
            .restart_blockers()
            .iter()
            .any(|b| b.kind == runtime::quiesce::RestartBlockerKind::ShellRunning)
        {
            changes.changed().await.unwrap();
        }
        let identity = shell.shell_job_snapshots(&run.to_string())[0]
            .identity
            .clone();
        {
            let mut abandoned = Box::pin(shell.wait_shell_job_terminal(&identity));
            assert!(futures_util::poll!(abandoned.as_mut()).is_pending());
        }
        let release = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(fifo)
                .unwrap()
                .write_all(b"released\n")
                .unwrap();
        });
        let terminal = shell.wait_shell_job_terminal(&identity).await.unwrap();
        release.await.unwrap();
        assert_eq!(terminal.exit_code, Some(0));
        assert!(!terminal.observed);
        while !h
            .runtime
            .restart_blockers()
            .iter()
            .any(|b| b.kind == runtime::quiesce::RestartBlockerKind::ShellResultUnobserved)
        {
            changes.changed().await.unwrap();
        }
        assert!(!matches!(
            h.runtime.quiesce_state(),
            runtime::quiesce::QuiesceState::Failed { .. }
        ));
        let mut events = Vec::new();
        let handle = loop {
            let event = h.receiver.recv().await.unwrap();
            let handle = match &event.kind {
                EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                    call_id,
                    detail: Some(detail),
                    ..
                }) if call_id == "shell-start" => {
                    detail["shell_job"]["job_id"].as_str().map(str::to_string)
                }
                _ => None,
            };
            events.push(event);
            if let Some(handle) = handle {
                break handle;
            }
        };
        h.mock.enqueue([
            ScriptedResponse::tool_call(
                "poll-response",
                MODEL,
                0,
                "shell-poll",
                "shell",
                [json!({"action":"poll","job_id":handle}).to_string()],
            ),
            text_response("first complete"),
            read_response(1),
            text_response("second complete"),
        ]);
        h.runtime.cancel_restart_quiesce(&attempt).unwrap();
        events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        assert!(shell.shell_job_snapshots(&run.to_string())[0].observed);
        h.runtime
            .send_message(run, "Continue after observation".into())
            .unwrap();
        events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        verify_trace(&h, run, &events, 0);
        h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}
