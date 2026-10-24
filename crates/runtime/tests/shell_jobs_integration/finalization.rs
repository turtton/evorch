#[path = "cancellation.rs"]
mod cancellation;

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};

struct GatedShell {
    shell: tools::tools::Shell,
    after_execute: Option<(mpsc::UnboundedSender<()>, tokio::sync::Semaphore)>,
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
        let result = self.shell.execute_with_context(ctx, args).await;
        if let Some((started, gate)) = &self.after_execute {
            started.send(()).unwrap();
            gate.acquire().await.unwrap().forget();
        }
        result
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
    test_support::isolated_state_dir();
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
        after_execute: None,
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
    test_support::isolated_state_dir();
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
    let mut previous = None;
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
        let number = job.strip_prefix("job-").unwrap().parse::<u64>().unwrap();
        assert!(previous.is_none_or(|previous| number > previous));
        previous = Some(number);
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
    test_support::isolated_state_dir();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_job_allows_mutation_reads_and_all_continuations() {
    test_support::isolated_state_dir();
    let (_temp, root) = init_git_repo();
    let bus = Arc::new(EventBus::new(256));
    let (started, _draining) = mpsc::unbounded_channel();
    let shell = Arc::new(GatedShell {
        shell: tools::tools::Shell::new(Arc::new(sandbox::DirectSandbox::new_unchecked())),
        after_execute: None,
        drains: AtomicUsize::new(1),
        started,
        gate: tokio::sync::Semaphore::new(0),
    });
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(shell.clone()).unwrap();
    executor.register(Arc::new(tools::Read)).unwrap();
    executor.register(Arc::new(tools::Write)).unwrap();
    let executor = Arc::new(executor);
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model);
    let run = runtime.delegate_background(
        Role::Worker,
        "fallback occupancy".into(),
        RunConfig::default(),
    );
    next(&mut calls).await.respond(tool_response(
        "start",
        "shell",
        json!({"command":"read first; read second", "yield_ms":0}),
    ));
    let call = next(&mut calls).await;
    let job = call.job("start");
    assert!(job.strip_prefix("job-").unwrap().parse::<u64>().is_ok());
    assert!(executor.has_running_shell_jobs(&run.to_string()));
    call.respond(tool_response(
        "write",
        "write",
        json!({"path":root.join("README.md"), "content":"written"}),
    ));
    let call = next(&mut calls).await;
    let (result, error) = call.result("write");
    assert!(!error, "{result}");
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "written"
    );
    assert!(executor.has_running_shell_jobs(&run.to_string()));
    call.respond(tool_response(
        "read",
        "read",
        json!({"path":root.join("README.md")}),
    ));
    let mut call = next(&mut calls).await;
    assert!(!call.result("read").1);
    for action in ["poll", "stdin", "stop"] {
        let mut input = json!({"action":action, "job_id":job, "yield_ms":0});
        if action == "stdin" {
            input["input"] = json!("first\n");
        }
        if action == "stop" {
            input["yield_ms"] = json!(1000);
        }
        call.respond(tool_response(action, "shell", input));
        call = next(&mut calls).await;
        assert_eq!(call.job(action), job);
        if action == "stop" {
            assert_eq!(field(&call.result(action).0, "status: "), "cancelled");
        } else {
            assert!(!call.result(action).1);
            assert!(executor.has_running_shell_jobs(&run.to_string()));
        }
    }
    call.respond(text_response("observed", FinishReason::Stop));
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Done);
}

#[cfg(unix)]
fn assert_handoff_blocked(permit: &runtime::ownership::OwnerPermit) {
    use runtime::ownership::Registry;
    use runtime::ownership::ipc::{Request, Response, request_with_timeout, serve_connection};
    let socket = permit.registry_path.with_extension("sock");
    let _ = std::fs::remove_file(&socket);
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let path = permit.registry_path.clone();
    let server = std::thread::spawn(move || {
        let mut registry = Registry::open_existing(&path).unwrap();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            serve_connection(&mut stream, &mut registry, 20).unwrap();
        }
    });
    // Ownership, not IPC speed: slow registry I/O must not close the client
    // before the server responds (which turns its reply into BrokenPipe).
    let ipc_timeout = Duration::from_secs(30);
    assert!(matches!(request_with_timeout(&socket, &Request::Quiesce {
        thread_id: permit.thread_id.clone(), token: permit.lease.clone(),
    }, ipc_timeout).unwrap(), Response::Status(owner) if owner.active_turn && owner.active_runs.len() == 1));
    assert!(matches!(
        request_with_timeout(
            &socket,
            &Request::Handoff {
                thread_id: permit.thread_id.clone(),
                token: permit.lease.clone(),
                successor_id: "successor".into(),
                settings: config::OwnershipConfig::default(),
            },
            ipc_timeout,
        )
        .unwrap(),
        Response::Rejected(_)
    ));
    server.join().unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrecorded_shell_round_retains_active_before_and_after_finalization() {
    test_support::isolated_state_dir();
    use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
    for mode in ["absent", "failed"] {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let permit = OwnerPermit {
            registry_path: dir.path().join("owners.db"),
            thread_id: "thread".into(),
            lease: Lease {
                owner_id: "owner".into(),
                generation: 1,
                expires_at: u64::MAX,
            },
            run_id: None,
        };
        Registry::open(&permit.registry_path)
            .unwrap()
            .start(&ThreadOwner::new("thread".into(), permit.lease.clone()))
            .unwrap();
        let bus = Arc::new(EventBus::new(256));
        let (executed, mut executing) = mpsc::unbounded_channel();
        let (started, mut draining) = mpsc::unbounded_channel();
        let shell = Arc::new(GatedShell {
            shell: tools::tools::Shell::new(Arc::new(sandbox::DirectSandbox::new_unchecked())),
            after_execute: Some((executed, tokio::sync::Semaphore::new(0))),
            drains: AtomicUsize::new(0),
            started,
            gate: tokio::sync::Semaphore::new(0),
        });
        let mut executor = ToolExecutor::new(bus.clone());
        executor.register(shell.clone()).unwrap();
        let executor = Arc::new(executor);
        let (model, mut calls) = model();
        let mut runtime = AgentRuntime::new(bus, executor.clone(), model);
        if mode == "failed" {
            runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        }
        let run = runtime.delegate_background(
            Role::Worker,
            "unknown effects".into(),
            RunConfig {
                ownership: Some(permit.clone()),
                ..Default::default()
            },
        );
        next(&mut calls).await.respond(tool_response(
            "start",
            "shell",
            json!({"command":"read reply", "yield_ms":0}),
        ));
        timeout(DEADLINE, executing.recv()).await.unwrap().unwrap();
        if mode == "failed" {
            rusqlite::Connection::open(&config.db_path).unwrap().execute_batch("CREATE TRIGGER reject_snapshot BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'reject round and terminal'); END;").unwrap();
        }
        shell.after_execute.as_ref().unwrap().1.add_permits(1);
        timeout(DEADLINE, draining.recv()).await.unwrap().unwrap();
        assert!(
            executor.has_unobserved_shell_jobs(&run.to_string()),
            "{mode}"
        );
        // Crucial: finalization has not persisted or released anything yet.
        assert_handoff_blocked(&permit);
        shell.gate.add_permits(1);
        assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error, "{mode}");
        assert!(executor.has_unobserved_shell_jobs(&run.to_string()));
        assert!(!executor.has_running_shell_jobs(&run.to_string()));
        assert_handoff_blocked(&permit);
        assert!(
            calls.try_recv().is_err(),
            "no next model round after failed checkpoint"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_store_rejecting_assistant_reference_is_fatal_in_round_and_finalize() {
    test_support::isolated_state_dir();
    for path in ["round", "finalize"] {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        rusqlite::Connection::open(&config.db_path).unwrap().execute_batch("CREATE TRIGGER reject_reference BEFORE INSERT ON assistant_references BEGIN SELECT RAISE(ABORT, 'mandatory reference rejected'); END;").unwrap();
        let bus = Arc::new(EventBus::new(256));
        let (executed, mut executing) = mpsc::unbounded_channel();
        let (started, mut draining) = mpsc::unbounded_channel();
        let shell = Arc::new(GatedShell {
            shell: tools::tools::Shell::new(Arc::new(sandbox::DirectSandbox::new_unchecked())),
            after_execute: Some((executed, tokio::sync::Semaphore::new(0))),
            drains: AtomicUsize::new(0),
            started,
            gate: tokio::sync::Semaphore::new(0),
        });
        let mut executor = ToolExecutor::new(bus.clone());
        executor.register(shell.clone()).unwrap();
        let executor = Arc::new(executor);
        let (model, mut calls) = model();
        let runtime = AgentRuntime::new(bus, executor.clone(), model);
        let run =
            runtime.delegate_background(Role::Worker, "late store".into(), RunConfig::default());
        let call = next(&mut calls).await;
        if path == "round" {
            runtime
                .capture_pending_reference(run, "ephemeral reference".into())
                .unwrap();
            call.respond(tool_response(
                "sync",
                "shell",
                json!({"command":"printf done", "yield_ms":1000}),
            ));
            timeout(DEADLINE, executing.recv()).await.unwrap().unwrap();
        } else {
            call.respond(text_response("ephemeral reference", FinishReason::Stop));
            timeout(DEADLINE, draining.recv()).await.unwrap().unwrap();
        }
        // AgentRuntime clones share the OnceLock, so this attaches to the running run.
        let runtime = runtime
            .clone()
            .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        if path == "round" {
            shell.after_execute.as_ref().unwrap().1.add_permits(1);
            timeout(DEADLINE, draining.recv()).await.unwrap().unwrap();
        }
        assert!(
            !executor.has_unobserved_shell_jobs(&run.to_string()),
            "no uncertain effects to mask Q2"
        );
        shell.gate.add_permits(1);
        assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error, "{path}");
        assert!(runtime.peek_pending_reference(run).unwrap().is_some());
        assert!(
            calls.try_recv().is_err(),
            "mandatory save failure must stop round"
        );
        assert!(
            storage::Database::open(&config)
                .unwrap()
                .run_context(&run.to_string())
                .unwrap()
                .is_none()
        );
    }
}
