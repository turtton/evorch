//! Integration of the real bootstrap/factories without a GUI, paid model or bwrap.
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
        "evaluation",
        "self-improvement",
        "mcp",
        "catalog",
        "pull-requests",
        "lane-stop-writes",
        "lane-recovery",
    ] {
        assert_eq!(
            services.iter().filter(|name| **name == required).count(),
            1,
            "service must be registered once: {required}"
        );
    }
    let permit = boot.owner.start("learning-thread").unwrap();
    let run = boot.runtime.delegate_background(
        Role::Worker,
        "evidence".into(),
        RunConfig {
            ownership: Some(permit),
            ..Default::default()
        },
    );
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
async fn gui_factory_and_core_partition_thread_and_global_work_on_one_database() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    let model = Arc::new(Model::default());
    let mut core = open(&dir, model.clone()).await;
    let (_store, gui, gui_owner, mut services) = gui_factory(&dir, &core, model.clone());
    let bus = gui.event_bus();
    let permit = core.owner.start("core-thread").unwrap();
    assert!(gui_owner.start("core-thread").is_err());
    let run = core.runtime.delegate_background(
        Role::Worker,
        "one owned source".into(),
        RunConfig {
            ownership: Some(permit),
            ..Default::default()
        },
    );
    core.runtime.wait(run).await.unwrap();
    core.runtime.wait_learning(run).await.unwrap().unwrap();
    assert_eq!(model.extracts.load(Ordering::SeqCst), 1);
    use eval::evaluator::EvaluatorHost;
    let gui_evaluator = evaluation::EvaluatorHost::default();
    gui_evaluator.runtime.set(gui.downgrade()).ok().unwrap();
    assert!(gui_evaluator.authorize_run(&run.to_string()).is_err());
    core.evaluator.authorize_run(&run.to_string()).unwrap();
    let _b = gui_owner.start("gui-thread").unwrap();
    for thread in [None, Some("core-thread"), Some("gui-thread")] {
        let event = diagnostic(thread);
        core.runtime.event_bus().emit(event.clone());
        bus.emit(event);
    }
    // A thread owned here cannot legitimize a run owned by the other host.
    let mut mismatched = diagnostic(Some("gui-thread"));
    if let event_bus::EventKind::Diagnostic(event) = &mut mismatched.kind {
        event.run_id = Some(run.to_string());
    }
    bus.emit(mismatched);
    services.shutdown().await.unwrap();
    core.shutdown().await.unwrap();
    let rows = candidates(root);
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| r.code == "NoProgress")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].occurrences, 3,
        "one global + one per owned thread, not six"
    );
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
    let permit = boot.owner.start("draining-learning").unwrap();
    let run = boot.runtime.delegate_background(
        Role::Worker,
        "completed evidence".into(),
        RunConfig {
            ownership: Some(permit),
            ..Default::default()
        },
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

fn gui_factory(
    dir: &test_socket_dir::SplitDir,
    core: &BootstrappedHost,
    model: Arc<Model>,
) -> (
    storage::Storage,
    AgentRuntime,
    Arc<OwnerHost>,
    services::BackgroundServices,
) {
    let db = components::storage_config(&core.config, dir.path().join("evorch-events.db"));
    learning_factory(dir, &core.config, model, db)
}

fn learning_factory(
    dir: &test_socket_dir::SplitDir,
    config: &config::Config,
    model: Arc<Model>,
    db: storage::StorageConfig,
) -> (
    storage::Storage,
    AgentRuntime,
    Arc<OwnerHost>,
    services::BackgroundServices,
) {
    // Construct precisely the factory the legacy GUI calls, on a second runtime.
    let bus = Arc::new(EventBus::new(64));
    let store = storage::Storage::open(db.clone()).unwrap();
    let gui_owner =
        Arc::new(OwnerHost::open(dir.sockets(), config.ownership.clone(), bus.clone()).unwrap());
    let gui = learning::configure(
        AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        ),
        config,
        db.clone(),
        store.handle(),
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
    learning::start(
        &mut services,
        &gui,
        gui_owner.clone(),
        config,
        &db,
        evaluation,
    )
    .unwrap();
    (store, gui, gui_owner, services)
}

#[tokio::test]
async fn review_m4_halted_writer_drain_preserves_spool_for_next_healthy_host() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    let mut config = config::Config::default();
    config.self_improvement.enabled = true;
    let mut db = components::storage_config(&config, root.join("evorch-events.db"));
    // The actual writer starts suspended, not a mock persistence failure.
    db.hard_limits.max_db_bytes = 0;
    let model = Arc::new(Model::default());
    let (store, runtime, owner, mut services) = learning_factory(&dir, &config, model.clone(), db);
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
    drop((services, runtime, owner, store));
    let mut next = open(&dir, model).await;
    next.shutdown().await.unwrap();
    assert!(!file.exists(), "acknowledged spool must be removed");
    let rows = candidates(root);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, "StorageWriterHalted");
    assert_eq!(rows[0].occurrences, 1);
}

#[tokio::test]
async fn review_m1_non_elected_bus_diagnostics_are_collected_without_broadcasting() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    let model = Arc::new(Model::default());
    let mut core = open(&dir, model.clone()).await;
    let (_store, gui, _owner, mut services) = gui_factory(&dir, &core, model);
    gui.event_bus().emit(diagnostic(None));
    gui.event_bus().emit(event_bus::Event::new(
        event_bus::FaultEvent::SkillDiagnostic {
            kind: event_bus::SkillDiagnosticKind::ValidationError,
            skill: "local-only".into(),
            scope: "project".into(),
            detail: "invalid metadata".into(),
        },
    ));
    services.shutdown().await.unwrap();
    core.shutdown().await.unwrap();
    let rows = candidates(root);
    for code in ["NoProgress", "SkillDiagnostic:ValidationError"] {
        let row = rows
            .iter()
            .find(|r| r.code == code)
            .unwrap_or_else(|| panic!("missing local {code}"));
        assert_eq!(row.occurrences, 1);
    }
}

#[tokio::test]
async fn review_m2_remaining_host_collects_events_and_spool_after_first_host_exits() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path();
    write_config(root);
    let model = Arc::new(Model::default());
    let mut first = open(&dir, model.clone()).await;
    let (_store, remaining, _owner, mut services) = gui_factory(&dir, &first, model);
    first.shutdown().await.unwrap();
    runtime::self_improvement::spool_fault(
        &root.join("self-improvement/drafts/crash-spool"),
        "StorageWriterHalted",
        "handover",
        "recovered after first owner exited",
    )
    .unwrap();
    remaining.event_bus().emit(diagnostic(None));
    services.shutdown().await.unwrap();
    let rows = candidates(root);
    for code in ["NoProgress", "StorageWriterHalted"] {
        assert!(
            rows.iter().any(|r| r.code == code),
            "missing {code} after owner exit"
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
        if invocation.run_id == "run-42" {
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
async fn review_m6_late_lane_admission_terminal_diagnostic_survives_shutdown() {
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
                start_seed:
                    json!({"run_id":"run-42", "project_root":root, "requires_ownership":true})
                        .to_string(),
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
    let mut observer = boot.services.lifecycle("self-improvement").unwrap();
    let (recovery_stopped, recovery_stop) = tokio::sync::oneshot::channel();
    let (release_recovery, recovery_gate) = tokio::sync::oneshot::channel();
    let runtime = boot.runtime.clone();
    boot.services.start("lane-recovery", |stop| async move {
        stop.await?;
        recovery_stopped.send(()).unwrap();
        recovery_gate.await?;
        model.release.notify_one();
        // Generation validation permits registration while quiescing, but the
        // initial turn then fails the owner fence. Wait for that real terminal
        // lifecycle event, not model entry (which must never happen here).
        let outcome = runtime.wait(run).await;
        assert!(
            runtime.inspect_agent(run).is_ok(),
            "late root registration failed: {outcome:?}"
        );
        Ok(())
    });
    let shutdown = tokio::spawn(async move {
        boot.shutdown().await.unwrap();
        boot
    });
    recovery_stop.await.unwrap();
    // If shutdown already stopped the observer (the regression), wait for its
    // actual completion before releasing either producer gate. Otherwise the
    // held recovery join proves stop cannot yet have reached the observer.
    // This drives the bad ordering without sleeps, yields, or timeouts.
    let observer_was_stopped = *observer.borrow() != ServicePhase::Running;
    if observer_was_stopped {
        observer
            .wait_for(|phase| *phase == ServicePhase::Completed)
            .await
            .unwrap();
    }
    release_recovery.send(()).unwrap();
    let boot = shutdown.await.unwrap();
    assert_eq!(
        boot.runtime.inspect_agent(run).unwrap().phase,
        event_bus::AgentRunPhase::Error
    );
    assert!(
        candidates(root)
            .iter()
            .any(|candidate| candidate.code.starts_with("RunError:")
                && candidate.run_id.as_deref() == Some("run-42")),
        "late registered root's terminal diagnostic must survive observer drain"
    );
}
