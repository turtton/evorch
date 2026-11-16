#[path = "shell_wait.rs"]
mod shell_wait;

use super::*;
use tokio::sync::{Notify, Semaphore};
use tools::{ShellJobIdentity, ShellJobSnapshot};

// Hold the actual shell result at the tool boundary so preparation cannot race
// the next provider request. No fake process/result is used.
struct GatedShell {
    shell: Arc<tools::Shell>,
    started: Notify,
    release: Semaphore,
    ack_blocked: std::sync::atomic::AtomicBool,
    teardown_blocked: std::sync::atomic::AtomicBool,
    ack_attempts: std::sync::atomic::AtomicUsize,
    snapshot_checked: Notify,
    ack_attempted: Notify,
    ack_changes: tokio::sync::watch::Sender<u64>,
}
impl GatedShell {
    fn new() -> Self {
        Self {
            shell: Arc::new(tools::Shell::new(Arc::new(
                sandbox::DirectSandbox::new_unchecked(),
            ))),
            started: Notify::new(),
            release: Semaphore::new(0),
            ack_blocked: false.into(),
            teardown_blocked: false.into(),
            ack_attempts: 0.into(),
            snapshot_checked: Notify::new(),
            ack_attempted: Notify::new(),
            ack_changes: tokio::sync::watch::channel(0).0,
        }
    }
    fn allow_ack(&self) {
        self.ack_blocked
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.ack_changes.send_modify(|v| *v += 1);
    }
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
    fn interrupt_shell_job(&self, identity: &ShellJobIdentity) -> Result<(), tools::ToolError> {
        self.shell.interrupt_shell_job(identity)
    }
    fn shell_job_result(
        &self,
        identity: &ShellJobIdentity,
    ) -> Result<tools::ShellJobResult, tools::ToolError> {
        self.shell.shell_job_result(identity)
    }
    fn acknowledge_shell_job_result(
        &self,
        identity: &ShellJobIdentity,
    ) -> Result<(), tools::ToolError> {
        self.ack_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.ack_blocked.load(std::sync::atomic::Ordering::SeqCst) {
            self.ack_attempted.notify_one();
            return Err(tools::ToolError::InvalidArgs {
                detail: "injected ACK failure".into(),
            });
        }
        let result = self.shell.acknowledge_shell_job_result(identity);
        if result.is_err() {
            self.ack_attempted.notify_one();
        }
        result
    }
    fn shell_job_snapshots(&self, run: &str) -> Vec<ShellJobSnapshot> {
        let mut jobs = self.shell.shell_job_snapshots(run);
        if self
            .teardown_blocked
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            for job in &mut jobs {
                job.teardown_confirmed = false;
            }
        }
        self.snapshot_checked.notify_one();
        jobs
    }
    fn subscribe_shell_job_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        let mut jobs = self.shell.subscribe_shell_job_changes()?;
        let mut faults = self.ack_changes.subscribe();
        let (tx, rx) = tokio::sync::watch::channel(0);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    result = jobs.changed() => { if result.is_err() { break; } }
                    result = faults.changed() => { if result.is_err() { break; } }
                }
                tx.send_modify(|v| *v += 1);
            }
        });
        Some(rx)
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
    fn take_shell_job_notifications(&self, run: &str) -> Vec<String> {
        if self
            .teardown_blocked
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Vec::new();
        }
        self.shell.take_shell_job_notifications(run)
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
    for (wire, interrupt, interactive) in [
        (Wire::ChatCompletions, false, false),
        (Wire::Responses, false, true),
        (Wire::Codex, false, false),
        (Wire::ChatCompletions, true, true),
        (Wire::Responses, true, false),
        (Wire::Codex, true, true),
    ] {
        let ShellFixture {
            _directory,
            _storage,
            config,
            mut h,
            shell,
            fifo,
            run,
            attempt,
        } = started_shell(wire, interactive).await;
        let identity = shell.shell_job_snapshots(&run.to_string())[0]
            .identity
            .clone();
        {
            let mut abandoned = Box::pin(shell.wait_shell_job_terminal(&identity));
            assert!(futures_util::poll!(abandoned.as_mut()).is_pending());
        }
        let jobs = h
            .runtime
            .restart_shell_jobs(&attempt, "shell-restart", "old-epoch")
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(
            h.runtime
                .restart_shell_jobs(&attempt, "other-restart", "old-epoch")
                .is_err()
        );
        assert!(
            h.runtime
                .interrupt_restart_shell_job(&attempt, "shell-restart", "wrong-epoch", &jobs[0])
                .await
                .is_err()
        );
        if interrupt {
            let terminal = h
                .runtime
                .interrupt_restart_shell_job(&attempt, "shell-restart", "old-epoch", &jobs[0])
                .await
                .unwrap();
            assert_eq!(terminal.state, tools::ShellJobState::Terminal);
            // Re-sending the same explicit operation is harmless, even after reap.
            h.runtime
                .interrupt_restart_shell_job(&attempt, "shell-restart", "old-epoch", &jobs[0])
                .await
                .unwrap();
        } else {
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
        }
        h.runtime.wait_quiesced().await.unwrap();
        let fence = h.runtime.fence_dispatch_restart().await.unwrap();
        let gate = runtime::quiesce::HostRestartGate::default();
        assert!(gate.transition(1, runtime::quiesce::HostRestartPhase::Sealing));
        let sealed = gate.sealed(&h.runtime, 1, fence).await.unwrap();
        drop(sealed); // No capture: cancellation is still legal.
        assert!(shell.shell_job_snapshots(&run.to_string())[0].observed);
        let record = storage::Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        let intent = h
            .runtime
            .resume_intents()
            .unwrap()
            .into_iter()
            .find(|intent| intent.run_id == run)
            .unwrap();
        assert!(
            intent
                .checkpoint
                .as_ref()
                .unwrap()
                .snapshot
                .available(Some(&record))
        );
        assert_eq!(
            record
                .messages_json
                .matches("Shell job terminal result (original call:")
                .count(),
            1
        );
        let outcome = shell.shell_job_result(&identity).unwrap();
        assert!(outcome.result.content.contains(if interrupt {
            "cancelled"
        } else {
            "normal:released"
        }));
        let mut events = Vec::new();
        loop {
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
            if handle.is_some() {
                break;
            }
        }
        h.mock.enqueue([
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
        let requests = h.mock.recorded_requests();
        let last = requests
            .iter()
            .rev()
            .find(|request| request.path == wire.path())
            .unwrap();
        assert_eq!(
            last.body
                .to_string()
                .matches("Shell job terminal result (original call:")
                .count(),
            1
        );
        assert!(
            h.runtime
                .interrupt_restart_shell_job(&attempt, "shell-restart", "old-epoch", &jobs[0])
                .await
                .is_err()
        );
        // A new attempt cannot reuse the preceding operation's opaque selection.
        let newer = h.runtime.begin_restart_quiesce().unwrap();
        h.runtime.wait_quiesced().await.unwrap();
        assert!(
            h.runtime
                .interrupt_restart_shell_job(&newer, "shell-restart", "old-epoch", &jobs[0])
                .await
                .is_err()
        );
        let record = storage::Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            record
                .messages_json
                .matches("Shell job terminal result (original call:")
                .count(),
            1
        );
        h.runtime.cancel_restart_quiesce(&newer).unwrap();
        h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}

struct ShellFixture {
    _directory: tempfile::TempDir,
    _storage: storage::Storage,
    config: storage::StorageConfig,
    h: Harness,
    shell: Arc<GatedShell>,
    fifo: std::path::PathBuf,
    run: RunId,
    attempt: runtime::quiesce::RestartAttempt,
}

async fn started_shell(wire: Wire, interactive: bool) -> ShellFixture {
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
    let shell = Arc::new(GatedShell::new());
    let command = json!({"command":format!("IFS= read -r line < '{}'; printf 'normal:%s' \"$line\"", fifo.display()),"yield_ms":0, "interactive":interactive});
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
    ShellFixture {
        _directory: directory,
        _storage: storage,
        config,
        h,
        shell,
        fifo,
        run,
        attempt,
    }
}

#[tokio::test]
async fn shell_terminal_snapshot_failure_keeps_observation_but_refuses_safety_ack() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let ShellFixture {
            _directory,
            _storage,
            config,
            mut h,
            shell,
            run,
            attempt,
            ..
        } = started_shell(wire, false).await;
        let before = storage::Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        let connection = rusqlite::Connection::open(&config.db_path).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_shell_checkpoint BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'checkpoint rejected'); END;").unwrap();
        let jobs = h
            .runtime
            .restart_shell_jobs(&attempt, "failed-shell-restart", "old-epoch")
            .unwrap();
        h.runtime
            .interrupt_restart_shell_job(&attempt, "failed-shell-restart", "old-epoch", &jobs[0])
            .await
            .unwrap();
        assert!(h.runtime.wait_quiesced().await.is_err());
        assert!(shell.shell_job_snapshots(&run.to_string())[0].observed);
        assert!(h.runtime.cancel_restart_quiesce(&attempt).is_err());
        assert!(h.runtime.fence_dispatch_restart().await.is_err());
        let after = storage::Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(after, before);
        let requests: Vec<_> = h
            .mock
            .recorded_requests()
            .into_iter()
            .filter(|request| request.path == h.wire.path())
            .collect();
        assert_eq!(requests.len(), 2);
        assert_append_only(wire.protocol(), &requests[0].body, &requests[1].body).unwrap();
        let mut usage = Vec::new();
        while usage.len() < 2 {
            if let EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) = h.receiver.recv().await.unwrap().kind
                && id == run.to_string()
            {
                usage.push((input_tokens, cache_read_tokens));
            }
        }
        assert!(usage[0].0 > 0);
        assert_eq!(usage[0].1, 0);
        assert!(
            usage[1].1 >= usage[0].0,
            "input-derived mock cache must reuse the sent prefix"
        );
        connection
            .execute_batch("DROP TRIGGER reject_shell_checkpoint;")
            .unwrap();
        h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}
