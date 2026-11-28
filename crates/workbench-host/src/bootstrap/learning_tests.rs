//! Integration of the real bootstrap/factories without a paid model or bwrap.
use super::*;
use async_trait::async_trait;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent, ToolSpec, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, Role, RunConfig, RunId, RuntimeError};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Model {
    extracts: AtomicUsize,
    hold_review: std::sync::atomic::AtomicBool,
    reviewing: tokio::sync::Notify,
}
fn result(messages: &[Message], id: &str) -> Option<Value> {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == id => {
                assert!(!is_error, "{content:?}");
                let ToolResultContent::Text { text } = &content[0] else {
                    panic!("text result")
                };
                Some(serde_json::from_str(text).unwrap())
            }
            _ => None,
        })
}
fn response(block: ContentBlock, reason: FinishReason) -> ChatResponse {
    ChatResponse {
        usage_reported: true,
        message: Message {
            role: providers::Role::Assistant,
            content: vec![block],
        },
        usage: Usage::default(),
        finish_reason: reason,
    }
}
fn tool(id: &str, name: &str, input: Value) -> ChatResponse {
    response(
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        },
        FinishReason::ToolUse,
    )
}
#[async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let source = || {
            messages
                .iter()
                .filter(|m| m.role == providers::Role::User)
                .flat_map(|m| &m.content)
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => text
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                        .find(|s| s.parse::<RunId>().is_ok())
                        .map(str::to_owned),
                    _ => None,
                })
                .next()
                .unwrap()
        };
        if role == Role::Evaluator && result(messages, "eval-list").is_none() {
            return Ok(tool("eval-list", "eval_list_threads", json!({})));
        }
        match invocation.category.as_deref() {
            Some("lesson") => {
                if result(messages, "source").is_none() {
                    self.extracts.fetch_add(1, Ordering::SeqCst);
                    return Ok(tool(
                        "source",
                        "inspect_learning_source",
                        json!({"run_id":source()}),
                    ));
                }
                if result(messages, "stack").is_none() {
                    let source = result(messages, "source").unwrap();
                    let reference =
                        source["records"].as_array().unwrap().last().unwrap()["reference"]
                            .as_str()
                            .unwrap();
                    return Ok(tool(
                        "stack",
                        "stack_lesson_candidate",
                        json!({"content":"Keep an explicit drain barrier", "evidence_refs":[reference], "scope":"harness"}),
                    ));
                }
            }
            Some("lesson_review") => {
                if self.hold_review.load(Ordering::SeqCst) {
                    self.reviewing.notify_one();
                    std::future::pending::<()>().await;
                }
                if result(messages, "list").is_none() {
                    return Ok(tool("list", "list_lesson_candidates", json!({})));
                }
                if result(messages, "source").is_none() {
                    return Ok(tool(
                        "source",
                        "inspect_learning_source",
                        json!({"run_id":source()}),
                    ));
                }
                if result(messages, "review").is_none() {
                    let listed = result(messages, "list").unwrap();
                    let candidate = &listed["candidates"][0];
                    return Ok(tool(
                        "review",
                        "submit_lesson_review",
                        json!({"candidate_id":candidate["id"], "verdict":"approve", "rationale":"Checked recorded source", "evidence_refs":candidate["evidence_refs"]}),
                    ));
                }
            }
            _ => {}
        }
        Ok(response(
            ContentBlock::Text {
                text: "Completed with evidence".into(),
            },
            FinishReason::Stop,
        ))
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "local/test".into()
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
fn options(dir: &test_socket_dir::SplitDir) -> BootstrapOptions {
    BootstrapOptions {
        config_dir: dir.path().into(),
        // Sockets live in the short directory so a long TMPDIR cannot exceed SUN_LEN.
        ownership_root: dir.sockets().into(),
        project_root: dir.path().into(),
    }
}
fn write_config(root: &std::path::Path) {
    std::fs::write(
        root.join("config.toml"),
        r#"
[providers.local]
type = "anthropic-subscription"
api_protocol = "anthropic-messages"
base_url = "http://127.0.0.1:1/v1"
credential = { type = "keyring", service = "evorch", account = "pr03b-unused" }
models = ["test"]
default_model = "test"
[[routing.routes.worker]]
profile = "local"
[agents.worker.categories.quick]
models = [{route = "worker"}]
[self_improvement]
enabled = true
[tools.browser]
enabled = false
"#,
    )
    .unwrap();
}
async fn open(dir: &test_socket_dir::SplitDir, model: Arc<Model>) -> BootstrappedHost {
    BootstrappedHost::open_with_test_seams(
        options(dir),
        TestSeams {
            model,
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap()
}
fn diagnostic(thread: Option<&str>) -> event_bus::Event {
    event_bus::Event::new(event_bus::DiagnosticEvent {
        source: "test".into(),
        severity: event_bus::DiagnosticSeverity::Warning,
        code: "NoProgress".into(),
        detail: "bounded diagnostic".into(),
        run_id: None,
        thread_id: thread.map(str::to_owned),
        call_id: None,
    })
}
fn candidates(root: &std::path::Path) -> Vec<storage::improvement::ImprovementCandidate> {
    storage::Database::open(&storage::StorageConfig {
        db_path: root.join("evorch-events.db"),
        ..Default::default()
    })
    .unwrap()
    .improvement_candidates(&runtime::repository::derive_repo_slug(root), None, 100)
    .unwrap()
}

#[tokio::test]
async fn headless_boot_runs_learning_evaluation_and_crash_intake_then_drains() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    let spool = root.join("self-improvement/drafts/crash-spool");
    runtime::self_improvement::spool_fault(
        &spool,
        "StorageWriterHalted",
        "writer",
        "writer stopped",
    )
    .unwrap();
    let model = Arc::new(Model::default());
    let mut boot = open(&dir, model.clone()).await;
    let services = boot.services.names().collect::<Vec<_>>();
    // All shared factories coexist; additional services must not break this
    // learning/drain contract or remove the lane producer shutdown participants.
    for required in [
        "admin-read-io",
        "admin-audit",
        "evaluation",
        "self-improvement",
        "mcp",
        "catalog",
        "pull-requests",
        "lane-recovery",
    ] {
        assert_eq!(
            services.iter().filter(|name| **name == required).count(),
            1,
            "service must be registered once: {required}"
        );
    }
    let run =
        boot.runtime
            .delegate_background(Role::Worker, "evidence".into(), RunConfig::default());
    boot.runtime.wait(run).await.unwrap();
    boot.runtime.wait_learning(run).await.unwrap().unwrap();
    assert_eq!(model.extracts.load(Ordering::SeqCst), 1);
    let eval = boot.runtime.delegate_background(
        Role::Evaluator,
        "inspect evaluations".into(),
        RunConfig {
            learning_internal: true,
            ..Default::default()
        },
    );
    assert_eq!(boot.runtime.wait(eval).await.unwrap(), AgentRunPhase::Done);
    // This event can still be in the bus when shutdown starts; drain must keep it.
    boot.runtime.event_bus().emit(diagnostic(None));
    boot.shutdown().await.unwrap();
    let rows = candidates(root);
    assert!(rows.iter().any(|r| r.code == "StorageWriterHalted"));
    assert!(rows.iter().any(|r| r.code == "NoProgress"));
    assert!(
        rows.iter()
            .any(|r| r.source == storage::improvement::ImprovementSource::Lesson)
    );
    assert!(std::fs::read_dir(spool).unwrap().next().is_none());
    assert!(
        storage::improvement_lease(&root.join("evorch-events.db"))
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn the_core_collects_each_diagnostic_once_for_its_own_work_only() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    let model = Arc::new(Model::default());
    let mut core = open(&dir, model.clone()).await;
    // A background run supplies the learning source; a chat root makes the thread known.
    let run =
        core.runtime
            .delegate_background(Role::Worker, "one source".into(), RunConfig::default());
    core.runtime.wait(run).await.unwrap();
    core.runtime.wait_learning(run).await.unwrap().unwrap();
    assert_eq!(model.extracts.load(Ordering::SeqCst), 1);
    let chat = core
        .runtime
        .delegate_chat(
            "core-thread",
            Role::Worker,
            "hello".into(),
            RunConfig::default(),
        )
        .unwrap();
    core.runtime.wait(chat).await.unwrap();
    use eval::evaluator::EvaluatorHost;
    core.evaluator.authorize_run(&run.to_string()).unwrap();
    // A run this core never ran cannot be named by an evaluation.
    assert!(core.evaluator.authorize_run("run-9999").is_err());
    let bus = core.runtime.event_bus();
    // Global events and the thread's own are kept, once each. A thread without a chat
    // root here, and a run that is not a background run of this core, are not its work.
    for thread in [None, Some("core-thread"), Some("unknown-thread")] {
        bus.emit(diagnostic(thread));
    }
    let mut foreign = diagnostic(Some("core-thread"));
    if let event_bus::EventKind::Diagnostic(event) = &mut foreign.kind {
        event.run_id = Some("run-9999".into());
    }
    bus.emit(foreign);
    bus.emit(event_bus::Event::new(
        event_bus::FaultEvent::SkillDiagnostic {
            kind: event_bus::SkillDiagnosticKind::ValidationError,
            skill: "local-only".into(),
            scope: "project".into(),
            detail: "invalid metadata".into(),
        },
    ));
    core.shutdown().await.unwrap();
    let rows = candidates(root);
    let no_progress: Vec<_> = rows.iter().filter(|r| r.code == "NoProgress").collect();
    assert_eq!(no_progress.len(), 1);
    assert_eq!(
        no_progress[0].occurrences, 2,
        "one global and one for the thread this core holds"
    );
    let skill = rows
        .iter()
        .find(|r| r.code == "SkillDiagnostic:ValidationError")
        .expect("the skill diagnostic is collected");
    assert_eq!(skill.occurrences, 1);
}

#[tokio::test]
async fn partial_startup_failure_releases_services_and_database_collector_lease() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    // Evaluation is already registered when the improvement factory hits this error.
    std::fs::create_dir_all(root.join("self-improvement")).unwrap();
    std::fs::write(root.join("self-improvement/drafts"), "not a directory").unwrap();
    let error = BootstrappedHost::open(options(&dir))
        .await
        .err()
        .expect("spool path must fail");
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotADirectory
    );
    assert!(
        storage::improvement_lease(&root.join("evorch-events.db"))
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn disabled_learning_evidence_path_is_inert_and_repeated_shutdown_drains() {
    let dir = test_socket_dir::SplitDir::new();
    let mut boot = BootstrappedHost::open(options(&dir)).await.unwrap();
    assert!(!dir.path().join("self-improvement").exists());
    boot.shutdown().await.unwrap();
    boot.shutdown().await.unwrap();
    assert!(!dir.path().join("self-improvement").exists());
}

#[tokio::test]
async fn evaluation_host_reload_uses_strict_saved_configuration() {
    use eval::evaluator::EvaluatorHost;
    let dir = test_socket_dir::SplitDir::new();
    write_config(dir.path());
    let mut boot = BootstrappedHost::open(options(&dir)).await.unwrap();
    boot.evaluator.reload_config().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[invalid").unwrap();
    assert!(boot.evaluator.reload_config().is_err());
    boot.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_drains_an_inflight_lesson_review_before_collector_and_lease_release() {
    let dir = test_socket_dir::SplitDir::new();
    write_config(dir.path());
    let model = Arc::new(Model::default());
    model.hold_review.store(true, Ordering::SeqCst);
    let mut boot = open(&dir, model.clone()).await;
    let run = boot.runtime.delegate_background(
        Role::Worker,
        "completed evidence".into(),
        RunConfig::default(),
    );
    boot.runtime.wait(run).await.unwrap();
    model.reviewing.notified().await;
    boot.shutdown().await.unwrap();
    assert!(boot.runtime.wait_learning(run).await.unwrap().is_err());
    assert!(
        candidates(dir.path())
            .iter()
            .any(|row| row.code == "LearningPipelineFailed")
    );
    assert!(
        storage::improvement_lease(&dir.path().join("evorch-events.db"))
            .unwrap()
            .is_some()
    );
}

fn learning_factory(
    dir: &test_socket_dir::SplitDir,
    config: &config::Config,
    model: Arc<Model>,
    db: storage::StorageConfig,
) -> (storage::Storage, AgentRuntime, services::BackgroundServices) {
    let store = storage::Storage::open(db.clone()).unwrap();
    let (runtime, services) = learning_factory_with_writer(dir, config, model, db, store.handle());
    (store, runtime, services)
}

// The learning factories of the core on a runtime of its own, over a given writer:
// never a second DB writer.
fn learning_factory_with_writer(
    dir: &test_socket_dir::SplitDir,
    config: &config::Config,
    model: Arc<Model>,
    db: storage::StorageConfig,
    writer: storage::StorageHandle,
) -> (AgentRuntime, services::BackgroundServices) {
    let bus = Arc::new(EventBus::new(64));
    let runtime = learning::configure(
        AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        ),
        config,
        db.clone(),
        writer,
        dir.path(),
    );
    let mut services = services::BackgroundServices::default();
    let evaluation = eval::evaluator::EvaluationService::default();
    services.start("evaluation", {
        let evaluation = evaluation.clone();
        |stop| async move {
            let _ = stop.await;
            evaluation.shutdown().await?;
            Ok(())
        }
    });
    learning::start(&mut services, &runtime, config, &db, evaluation).unwrap();
    (runtime, services)
}

#[tokio::test]
async fn review_m4_halted_writer_drain_preserves_spool_for_next_healthy_host() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    let mut config = config::Config::default();
    config.self_improvement.enabled = true;
    let db = components::storage_config(&config, root.join("evorch-events.db"));
    let model = Arc::new(Model::default());
    let (store, runtime, mut services) = learning_factory(&dir, &config, model.clone(), db);
    // The actual writer refuses candidate writes, not a mock persistence failure.
    let raw = rusqlite::Connection::open(root.join("evorch-events.db")).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER fail_candidate BEFORE INSERT ON improvement_candidates \
         BEGIN SELECT RAISE(ABORT, 'test writer failure'); END;",
    )
    .unwrap();
    let spool = root.join("self-improvement/drafts/crash-spool");
    runtime::self_improvement::spool_fault(
        &spool,
        "StorageWriterHalted",
        "storage:db_size_limit",
        "event writes halted by storage limit",
    )
    .unwrap();
    let file = std::fs::read_dir(&spool)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    services.shutdown().await.unwrap();
    assert!(
        file.exists(),
        "unacknowledged spool must survive the final drain"
    );
    assert!(candidates(root).is_empty());
    drop((services, runtime, store));
    raw.execute_batch("DROP TRIGGER fail_candidate").unwrap();
    drop(raw);
    let mut next = open(&dir, model).await;
    next.shutdown().await.unwrap();
    assert!(!file.exists(), "acknowledged spool must be removed");
    let rows = candidates(root);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, "StorageWriterHalted");
    assert_eq!(rows[0].occurrences, 1);
}

#[tokio::test]
async fn review_m2_next_learning_service_collects_events_and_spool_after_the_first_exits() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    let model = Arc::new(Model::default());
    let mut config = config::Config::default();
    config.self_improvement.enabled = true;
    let db = components::storage_config(&config, root.join("evorch-events.db"));
    // The collector holds no lease while it runs, so the service that starts next over
    // the same writer takes over events and the spool the first one left.
    let (store, first, mut first_services) =
        learning_factory(&dir, &config, model.clone(), db.clone());
    first_services.shutdown().await.unwrap();
    drop((first_services, first));
    let (remaining, mut services) =
        learning_factory_with_writer(&dir, &config, model, db, store.handle());
    runtime::self_improvement::spool_fault(
        &root.join("self-improvement/drafts/crash-spool"),
        "StorageWriterHalted",
        "handover",
        "recovered after the first service exited",
    )
    .unwrap();
    remaining.event_bus().emit(diagnostic(None));
    services.shutdown().await.unwrap();
    let rows = candidates(root);
    for code in ["NoProgress", "StorageWriterHalted"] {
        assert!(
            rows.iter().any(|r| r.code == code),
            "missing {code} after the first service exited"
        );
    }
}

#[tokio::test]
async fn review_m3_evaluation_work_prevents_idle_exit_until_it_is_drained() {
    let dir = test_socket_dir::SplitDir::new();
    let mut core = BootstrappedHost::open(options(&dir)).await.unwrap();
    assert!(!core.busy());
    let sh = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("sh"))
        .find(|p| p.is_file())
        .unwrap();
    let mut child = tokio::process::Command::new(sh)
        .args(["-c", "kill -STOP $$"])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    core.evaluations
        .spawn(async move {
            let _ = released.await;
            let _ = child.start_kill();
            child.wait().await.unwrap();
        })
        .unwrap();
    let busy = core.busy();
    release.send(()).unwrap();
    core.evaluations.shutdown().await.unwrap();
    let drained = !core.busy();
    core.shutdown().await.unwrap();
    assert!(
        busy,
        "an evaluation child must keep the core alive without any active agent"
    );
    assert!(
        drained,
        "completed evaluation work must not keep the core alive"
    );
}

#[derive(Default)]
struct LateLaneAdmission {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    /// Only the lane admission waits; the turns admitted after it pass.
    admitted: std::sync::atomic::AtomicBool,
    lessons: Model,
}
#[async_trait]
impl AgentModel for LateLaneAdmission {
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
    ) -> Result<(), RuntimeError> {
        if invocation.run_id == "run-42" && !self.admitted.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.lessons
            .complete(invocation, role, messages, tools)
            .await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "local/test".into()
    }
}

#[tokio::test]
async fn review_m6_late_lane_root_is_stopped_by_the_drain_while_the_observer_runs() {
    use super::services::ServicePhase;
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    {
        let storage = storage::Storage::open(storage::StorageConfig {
            db_path: root.join("evorch-events.db"),
            ..Default::default()
        })
        .unwrap();
        storage
            .handle()
            .create_lane(storage::lanes::NewLane {
                lane_id: "late-recovery".into(),
                thread_id: "late-lane-thread".into(),
                origin_thread_id: "lead".into(),
                origin_run_id: "run-41".into(),
                creation_key: "before-crash".into(),
                brief: serde_json::from_value(json!({
                    "objective":"resume durable intent", "inputs":[], "deliverables":["patch"],
                    "acceptance_criteria":["review"], "constraints":[], "workspace":{},
                    "report_policy":"attached"
                }))
                .unwrap(),
                start_seed: json!({"run_id":"run-42", "project_root":root}).to_string(),
                project_id: None,
                project_root: Some(root.to_string_lossy().into_owned()),
                title: None,
                at_ns: 1,
            })
            .unwrap();
    }
    let model = Arc::new(LateLaneAdmission::default());
    let mut boot = BootstrappedHost::open_with_test_seams(
        options(&dir),
        TestSeams {
            model: model.clone(),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    model.entered.notified().await;
    let run: RunId = "run-42".parse().unwrap();
    assert!(boot.runtime.list_agents().is_empty());
    assert_eq!(boot.runtime.pending_lane_admissions(), [run]);
    // Real startup recovered the durable intent. Keep that producer's completion
    // gated while its provider admission is still outside the RunEntry snapshot.
    boot.services.join_named("lane-recovery").await.unwrap();
    let observer = boot.services.lifecycle("self-improvement").unwrap();
    let (recovery_stopped, recovery_stop) = tokio::sync::oneshot::channel();
    let (release_recovery, recovery_gate) = tokio::sync::oneshot::channel();
    let runtime = boot.runtime.clone();
    boot.services.start("lane-recovery", |stop| async move {
        stop.await?;
        recovery_stopped.send(()).unwrap();
        recovery_gate.await?;
        let mut events = runtime.event_bus().subscribe();
        model.release.notify_one();
        // Registration is still permitted while the dispatch fence is closing. Wait for
        // its real lifecycle event, not for the root's end: the fence holds the root
        // until the drain stops it, after this producer has completed.
        loop {
            let event = events.recv().await.unwrap();
            if matches!(&event.kind, event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged { run_id, .. }) if run_id == "run-42")
            {
                break;
            }
        }
        assert!(
            runtime.inspect_agent(run).is_ok(),
            "late root registration failed"
        );
        Ok(())
    });
    let shutdown = tokio::spawn(async move {
        boot.shutdown().await.unwrap();
        boot
    });
    recovery_stop.await.unwrap();
    // The held recovery join proves shutdown cannot have stopped the learning observer
    // yet: it must outlive every producer that can still register a root. This is
    // checked without sleeps, yields, or timeouts.
    assert!(
        *observer.borrow() == ServicePhase::Running,
        "the learning observer must stay subscribed while a lane producer can register"
    );
    release_recovery.send(()).unwrap();
    let boot = shutdown.await.unwrap();
    assert_eq!(
        boot.runtime.inspect_agent(run).unwrap().phase,
        event_bus::AgentRunPhase::Stopped,
        "the drain stops the root that registered while the producers were stopping"
    );
}
