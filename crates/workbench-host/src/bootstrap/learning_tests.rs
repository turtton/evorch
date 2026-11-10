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
fn options(root: &std::path::Path) -> BootstrapOptions {
    BootstrapOptions {
        config_dir: root.into(),
        ownership_root: root.join("owners"),
        project_root: root.into(),
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
async fn open(root: &std::path::Path, model: Arc<Model>) -> BootstrappedHost {
    BootstrappedHost::open_with_test_seams(
        options(root),
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
    let dir = tempfile::tempdir().unwrap();
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
    let mut boot = open(root, model.clone()).await;
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
        assert!(services.contains(&required), "missing service: {required}");
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
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_config(root);
    let model = Arc::new(Model::default());
    let mut core = open(root, model.clone()).await;
    // Construct precisely the factory the legacy GUI calls, on a second runtime.
    let bus = Arc::new(EventBus::new(64));
    let db = components::storage_config(&core.config, root.join("evorch-events.db"));
    let store = storage::Storage::open(db.clone()).unwrap();
    let gui_owner = Arc::new(
        OwnerHost::open(
            &root.join("owners"),
            core.config.ownership.clone(),
            bus.clone(),
        )
        .unwrap(),
    );
    let gui = learning::configure(
        AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        ),
        &core.config,
        db.clone(),
        store.handle(),
        root,
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
        &core.config,
        &db,
        evaluation,
    )
    .unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_config(root);
    // Evaluation is already registered when the improvement factory hits this error.
    std::fs::create_dir_all(root.join("self-improvement")).unwrap();
    std::fs::write(root.join("self-improvement/drafts"), "not a directory").unwrap();
    let error = BootstrappedHost::open(options(root))
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
    let dir = tempfile::tempdir().unwrap();
    let mut boot = BootstrappedHost::open(options(dir.path())).await.unwrap();
    assert!(!dir.path().join("self-improvement").exists());
    boot.shutdown().await.unwrap();
    boot.shutdown().await.unwrap();
    assert!(!dir.path().join("self-improvement").exists());
}

#[tokio::test]
async fn evaluation_host_reload_uses_strict_saved_configuration() {
    use eval::evaluator::EvaluatorHost;
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path());
    let mut boot = BootstrappedHost::open(options(dir.path())).await.unwrap();
    boot.evaluator.reload_config().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[invalid").unwrap();
    assert!(boot.evaluator.reload_config().is_err());
    boot.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_drains_an_inflight_lesson_review_before_collector_and_lease_release() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path());
    let model = Arc::new(Model::default());
    model.hold_review.store(true, Ordering::SeqCst);
    let mut boot = open(dir.path(), model.clone()).await;
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
