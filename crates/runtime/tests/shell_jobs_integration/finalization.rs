#[path = "cancellation.rs"]
mod cancellation;

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};

struct GatedShell {
    shell: tools::tools::Shell,
    drains: AtomicUsize,
    started: mpsc::UnboundedSender<()>,
    gate: tokio::sync::Semaphore,
}
#[async_trait]
impl Tool for GatedShell {
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
        self.shell.execute_with_context(ctx, args).await
    }
    fn cancel_shell_jobs(&self, run: &str) {
        self.shell.cancel_shell_jobs(run);
    }
    async fn drain_shell_jobs(&self, run: &str) -> Result<(), ToolError> {
        if self.drains.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.send(()).unwrap();
            self.gate.acquire().await.unwrap().forget();
        }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_publication_waits_for_drain_and_same_id_continuation_keeps_new_context_and_job() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let (started, mut draining) = mpsc::unbounded_channel();
    let shell = Arc::new(GatedShell {
        shell: tools::tools::Shell::new(Arc::new(sandbox::DirectSandbox::new_unchecked())),
        drains: AtomicUsize::new(0),
        started,
        gate: tokio::sync::Semaphore::new(0),
    });
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(shell.clone()).unwrap();
    let executor = Arc::new(executor);
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(
        Role::Worker,
        "first generation".into(),
        RunConfig::default(),
    );
    next(&mut calls)
        .await
        .respond(text_response("first result", FinishReason::Stop));
    timeout(DEADLINE, draining.recv()).await.unwrap().unwrap();
    assert_eq!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Running
    );
    assert!(runtime.wait(run).now_or_never().is_none());
    for event in support::drain_events(&mut events).await {
        assert!(!matches!(
            event.kind,
            event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Done | AgentRunPhase::Error,
                    ..
                } | event_bus::LifecycleEvent::BackgroundTaskCompleted { .. }
                    | event_bus::LifecycleEvent::BackgroundTaskCancelled { .. }
            )
        ));
    }
    shell.gate.add_permits(1);
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Done);
    assert_eq!(
        runtime
            .continue_goal(run, "second generation".into(), RunConfig::default())
            .unwrap(),
        run
    );
    let call = next(&mut calls).await;
    assert!(
        serde_json::to_string(&call.messages)
            .unwrap()
            .contains("second generation")
    );
    call.respond(tool_response(
        "second-start",
        "shell",
        json!({"command":"read reply", "yield_ms":0}),
    ));
    let call = next(&mut calls).await;
    let job = call.job("second-start");
    assert!(executor.has_running_shell_jobs(&run.to_string()));
    assert_eq!(
        shell.drains.load(Ordering::SeqCst),
        1,
        "old finalization cannot revisit the successor"
    );
    let saved = storage::Database::open(&config)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    assert!(saved.messages_json.contains("second generation"));
    assert!(
        runtime
            .restore_diagnostics(run)
            .unwrap()
            .unwrap()
            .interrupted_tool_calls
            .iter()
            .any(|c| c.call_id == "unobserved-shell-jobs")
    );
    call.respond(tool_response(
        "second-stop",
        "shell",
        json!({"action":"stop", "job_id":job, "yield_ms":1000}),
    ));
    let call = next(&mut calls).await;
    assert_eq!(
        field(&call.result("second-stop").0, "status: "),
        "cancelled"
    );
    runtime.cancel(run).unwrap();
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_than_retained_capacity_cancelled_runs_release_handles_and_keep_durable_uncertainty() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let executor = executor(&bus, dir.path());
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    for index in 0..35 {
        let run = runtime.delegate_background(
            Role::Worker,
            format!("cancel generation {index}"),
            RunConfig::default(),
        );
        next(&mut calls).await.respond(tool_response(
            "start",
            "shell",
            json!({"command":"read reply", "yield_ms":0}),
        ));
        let call = next(&mut calls).await;
        let job = call.job("start");
        assert!(!call.result("start").1, "run {index} must fit the registry");
        runtime.cancel(run).unwrap();
        assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error);
        assert!(!executor.has_running_shell_jobs(&run.to_string()));
        assert!(!executor.has_unobserved_shell_jobs(&run.to_string()));
        let diagnostics = runtime.restore_diagnostics(run).unwrap().unwrap();
        assert!(!diagnostics.disk_restorable);
        assert!(
            diagnostics
                .interrupted_tool_calls
                .iter()
                .any(|c| c.call_id == "unobserved-shell-jobs" && c.may_have_side_effects)
        );
        assert!(diagnostics.history_available_with_current_authority);
        let poll = executor
            .execute(
                &ToolExecutionContext {
                    run_id: run.to_string(),
                    thread_id: None,
                    call_id: None,
                },
                "shell",
                "expired",
                json!({"action":"poll", "job_id":job}),
            )
            .await;
        assert!(poll.is_err(), "terminal handle must be unavailable");
        drop(call);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_terminal_snapshot_keeps_unobserved_handles_and_prior_uncertainty() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let executor = executor(&bus, dir.path());
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(
        Role::Worker,
        "preserve unknown effects".into(),
        RunConfig::default(),
    );
    next(&mut calls).await.respond(tool_response(
        "start",
        "shell",
        json!({"command":"read reply", "yield_ms":0}),
    ));
    let call = next(&mut calls).await;
    assert!(!call.result("start").1);
    let before = runtime.restore_diagnostics(run).unwrap().unwrap();
    assert!(!before.disk_restorable);
    storage.close();
    runtime.cancel(run).unwrap();
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error);
    assert!(!executor.has_running_shell_jobs(&run.to_string()));
    assert!(
        executor.has_unobserved_shell_jobs(&run.to_string()),
        "failed final persistence must not discard unknown results"
    );
    assert_eq!(runtime.restore_diagnostics(run).unwrap().unwrap(), before);
    assert!(support::drain_events(&mut events).await.iter().any(|event| {
        matches!(&event.kind, event_bus::EventKind::Diagnostic(d) if d.code == "ContextSnapshotFailed")
    }));
    drop(call);
}
