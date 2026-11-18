//! `evorch eval run` through the core keeps the prompt-cache contract of the
//! replay it drives (ADR 0045 §10, stage 17c).
//!
//! The replay of a recorded thread runs as a counted job of the core's
//! `evaluation` area: the CLI only starts it and follows it. The job composes the
//! replay's runtime from the user's configuration, so its requests cross the
//! production routing and HTTP adapters and are read back at the HTTP boundary of
//! the offline mock, whose prompt cache is derived from the request bytes.
//!
//! * The replay's agent requests are one conversation: the second extends the
//!   first (same model, cache key, tool schema and prefix) however the job was
//!   started, and the job adds nothing to what the model is sent.
//! * The simulated user's requests are independent structured calls; they carry
//!   a schema and are never mixed into the agent's history.
//! * A core that was bootstrapped (the production composition) answers an
//!   evaluation command; the area is wired and not "unsupported".
//!
//! The host of the first scenario is assembled by the test the way the host's own
//! tests do, with the production `evaluation` area and only its tool isolation
//! switched off: the bootstrap offers no seam for the isolation of a replay, and
//! a replay under the production sandbox needs the host's bubblewrap. No test
//! waits for time: a job ends with the settled event it announces.
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use sandbox::CredentialStore as _;
use serde_json::Value;
use storage::thread_start::{ThreadStartCapture, ThreadStartSnapshot, ThreadStartState};
use storage::{ReviewEvidenceRow, RunContextRecord, Storage, StorageConfig};
use workbench_host::CommandHost;
use workbench_host::areas::evaluation::{
    EvaluationArea, FixedModels, Isolation, JobParts, JobSeams,
};
use workbench_host::areas::{AreaTasks, Areas};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::commands::CommandService;
use workbench_host::projects::StaticProjects;
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::Command;
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::{OperationId, ProjectId, ThreadId};
use workbench_protocol::namespaces::evaluation::{
    CancelEvaluationJob, EvaluationCommand, EvaluationEvent, EvaluationJobEffect,
    EvaluationJobView, EvaluationRunStatus, StartEvaluationReport, StartEvaluationRun,
};
use workbench_protocol::operation::{CancelReason, OperationState};

#[allow(dead_code)]
#[path = "../../runtime/tests/mcp_cache_support/mod.rs"]
mod cache;

const MODEL: &str = "mock-model";
const THREAD: &str = "thread-1";
const ACCOUNT: &str = "eval-cache-key";
const AGENT_PATH: &str = "/v1/chat/completions";
const FINISH: &str = r#"{"action":"finish","text":"","reason":"the request is met"}"#;
const REPLY: &str = r#"{"action":"reply","text":"please go on","reason":"scripted"}"#;

struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

/// A model that never answers: the runtime of the assembled host needs one, and
/// no chat is started in these scenarios.
struct Silent;

#[async_trait::async_trait]
impl runtime::AgentModel for Silent {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "silent".into()
    }
}

fn git(project: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(project)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

/// A recorded thread: the repository it started from, and what is stored of it.
fn record_thread(root: &Path, database: &Path) {
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    git(&project, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(project.join("README"), "replay fixture\n").unwrap();
    git(&project, &["add", "README"]);
    git(&project, &["commit", "--quiet", "-m", "fixture"]);
    let storage = Storage::open(StorageConfig {
        db_path: database.to_path_buf(),
        ..Default::default()
    })
    .unwrap();
    let writer = storage.handle();
    writer
        .save_thread_start_state(ThreadStartState {
            thread_id: THREAD.into(),
            root_run_id: "run-1".into(),
            project_root: project.to_string_lossy().into_owned(),
            captured_at_ns: 1,
            capture: ThreadStartCapture::Replayable(ThreadStartSnapshot {
                repo_prefix: String::new(),
                head_commit: git(&project, &["rev-parse", "HEAD"]),
                head_ref: Some("refs/heads/main".into()),
                diff: Vec::new(),
                untracked: Vec::new(),
            }),
        })
        .unwrap();
    writer
        .upsert_run_context(&RunContextRecord {
            run_id: "run-1".into(),
            name: format!("chat:{}:{THREAD}", runtime::Role::Worker.name()),
            role: runtime::Role::Worker.name().into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: "[]".into(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Done".into(),
            restorable: false,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        })
        .unwrap();
    writer
        .record_review_evidence(&[ReviewEvidenceRow {
            id: "prompt-1".into(),
            root_run_id: "run-1".into(),
            target_run_id: "run-1".into(),
            kind: "initial_prompt".into(),
            text: "fix the bug".into(),
            in_reply_to_question_id: None,
            in_reply_to_title: None,
            reference_kind: None,
            reference_context: None,
            attachments: Vec::new(),
            created_at_ns: 1,
        }])
        .unwrap();
    storage.close();
}

/// The user's configuration: one provider in front of the mock, whose key is
/// kept in the credential store the way a saved provider's is.
fn configure(config_dir: &Path, base_url: &str) {
    std::fs::create_dir_all(config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"version = {version}
[providers.local]
type = "openai-compatible"
api_protocol = "openai-completions"
base_url = "{base_url}"
credential = {{ type = "keyring", service = "evorch", account = "{ACCOUNT}" }}
models = ["{MODEL}"]
default_model = "{MODEL}"
[routing.routes]
worker = [{{ profile = "local" }}]
"#,
            version = config::CURRENT_VERSION,
        ),
    )
    .unwrap();
    sandbox::FileCredentialStore::open(config_dir)
        .unwrap()
        .set(ACCOUNT, &sandbox::Secret::from("fixture-key".to_owned()))
        .unwrap();
}

fn operation(id: &str) -> OperationId {
    OperationId::new(id).unwrap()
}

fn run_of(id: &str, thread: &str) -> StartEvaluationRun {
    StartEvaluationRun {
        operation_id: operation(id),
        thread: ThreadId::new(thread).unwrap(),
        candidate: None,
        repeat: Some(1),
        output: Some("cache-run".into()),
        problem: None,
        verify: None,
        verify_timeout_secs: None,
        keep_workspace: false,
        report: false,
    }
}

fn subscribe(host: &CommandHost) -> workbench_host::subscriptions::Subscription {
    host.projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap()
}

/// The settled event of the job `id`: the core's own signal that it ended.
async fn settled(
    events: &mut workbench_host::subscriptions::Subscription,
    id: &str,
) -> EvaluationJobView {
    loop {
        let UiEvent::Evaluation(event) =
            events.recv().await.expect("the subscription is open").event
        else {
            continue;
        };
        if let EvaluationEvent::EvaluationJobSettled { view } = event
            && view.operation.operation_id.as_str() == id
        {
            return *view;
        }
    }
}

/// Wait until the counted task of the job ended: the job reports its end
/// before the task drops what it holds, so a view alone does not say it.
async fn idle(tasks: &AreaTasks) {
    let ended = tasks.counted_ended_notify();
    while tasks.busy() {
        ended.notified().await;
    }
}

/// The host of the first scenarios: the production `evaluation` area over the
/// recorded database, with only the isolation of the replays' tools switched off.
fn job_host(root: &Path, config_dir: &Path, database: &Path) -> (CommandHost, AreaTasks) {
    job_host_with(root, config_dir, database, None)
}

fn job_host_with(
    root: &Path,
    config_dir: &Path,
    database: &Path,
    models: Option<FixedModels>,
) -> (CommandHost, AreaTasks) {
    let bus = Arc::new(event_bus::EventBus::new(64));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(Silent),
    );
    let mut projects = StaticProjects::default();
    projects.insert(&ProjectId::new("project-1").unwrap(), root.join("project"));
    let host = CommandHost::new(CommandService::new(
        runtime,
        tokio::runtime::Handle::current(),
    ))
    .with_projects(projects);
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let evaluation = EvaluationArea::from_job_parts(JobParts {
        tasks: tasks.clone(),
        source: host.projection().source("evaluation").unwrap(),
        root: config_dir.join("eval"),
        config_dir: config_dir.to_path_buf(),
        database: database.to_path_buf(),
        credentials: Arc::new(sandbox::FileCredentialStore::open(config_dir).unwrap()),
        seams: JobSeams {
            isolation: Isolation::DirectUnchecked,
            models,
            load_catalog: false,
            ..JobSeams::default()
        },
    });
    let host = host.with_areas(Areas {
        evaluation,
        ..Default::default()
    });
    (host, tasks)
}

fn text(id: &str, fragment: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream(id, MODEL, [fragment])
}

fn is_structured(request: &Value) -> bool {
    request.get("response_format").is_some()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replay_started_through_the_core_keeps_the_sent_prefix_on_the_production_wire() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_dir = root.join("config");
    let database = root.join("events.db");
    record_thread(&root, &database);
    // The agent answers, the simulated user asks it to go on, the agent answers
    // again, and the simulated user ends the conversation.
    let provider = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            text("agent-1", "I looked at the bug."),
            text("user-1", REPLY),
            text("agent-2", "I fixed the bug."),
            text("user-2", FINISH),
        ],
        vec![MODEL.into()],
    );
    configure(&config_dir, &provider.base_url());

    let (mut host, tasks) = job_host(&root, &config_dir, &database);
    let mut events = subscribe(&host);

    host.handle_command(
        Command::Evaluation(EvaluationCommand::StartEvaluationRun(Box::new(run_of(
            "replay", THREAD,
        )))),
        local_desktop(),
    )
    .unwrap();
    let done = settled(&mut events, "replay").await;
    assert_eq!(done.operation.state, OperationState::Ready, "{done:?}");
    assert_eq!(done.runs.len(), 1, "{done:?}");
    assert_eq!(done.runs[0].status, EvaluationRunStatus::Completed);
    assert_eq!(done.runs[0].user_turns, 2, "two user turns were replayed");
    provider_drained(&provider);

    let requests: Vec<Value> = provider
        .recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == AGENT_PATH)
        .map(|request| request.body)
        .collect();
    let (simulator, agent): (Vec<_>, Vec<_>) = requests.iter().partition(|r| is_structured(r));
    assert_eq!(agent.len(), 2, "one request per agent turn");
    assert_eq!(
        simulator.len(),
        2,
        "one request per simulated user decision"
    );
    assert_append_only(CacheProtocol::OpenAi, agent[0], agent[1])
        .expect("the second turn keeps the sent prefix and the settings");
    for field in ["model", "prompt_cache_key", "tools"] {
        assert_eq!(agent[0][field], agent[1][field], "{field}");
    }
    assert!(
        agent[1]["messages"].as_array().unwrap().len()
            > agent[0]["messages"].as_array().unwrap().len(),
        "the second turn only appends"
    );
    for request in &simulator {
        assert!(
            request["response_format"]["json_schema"]
                .to_string()
                .contains("additionalProperties"),
            "a schema is requested"
        );
    }
    idle(&tasks).await;
}

fn provider_drained(provider: &StreamingMockOpenAi) {
    assert_eq!(
        provider.remaining_scripts(),
        0,
        "every scripted answer was used"
    );
}

/// The production composition of the core wires the evaluation area: a job it
/// is asked for ends with its own reason instead of being refused as unsupported.
#[tokio::test(flavor = "multi_thread")]
async fn a_bootstrapped_core_runs_an_evaluation_job_and_names_why_it_failed() {
    let project = cache::Project::new();
    let root = project.root().canonicalize().unwrap();
    let user = root.join("user");
    configure(&user, "http://127.0.0.1:9");
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: user.clone(),
            project_root: root.clone(),
            ownership_root: project.sockets().to_path_buf(),
        },
        TestSeams {
            model: Arc::new(Silent),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();
    let mut events = subscribe(&core.host.lock().unwrap());
    core.host
        .lock()
        .unwrap()
        .handle_command(
            Command::Evaluation(EvaluationCommand::StartEvaluationRun(Box::new(run_of(
                "missing",
                "thread-that-was-never-recorded",
            )))),
            local_desktop(),
        )
        .unwrap();
    let done = settled(&mut events, "missing").await;
    assert!(
        matches!(done.operation.state, OperationState::Failed { .. }),
        "{done:?}"
    );
    let message = done.message.expect("the failure says why");
    assert!(
        message.contains("thread-that-was-never-recorded"),
        "{message}"
    );
    assert!(
        !message.contains(&*root.to_string_lossy()),
        "no host path leaves the core: {message}"
    );
    core.shutdown().await.unwrap();
}

/// The user's configuration for the preflight scenarios: the provider offers
/// `gpt-4o` only, and the reviewer route may name a model it does not.
fn configure_preflight(config_dir: &Path, base_url: &str) {
    configure(config_dir, base_url);
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"version = {version}
[providers.local]
type = "openai-compatible"
api_protocol = "openai-completions"
base_url = "{base_url}"
credential = {{ type = "keyring", service = "evorch", account = "{ACCOUNT}" }}
models = ["gpt-4o", "missing"]
default_model = "gpt-4o"
[routing.routes]
worker = [{{ profile = "local" }}]
reviewer = [{{ profile = "local" }}]
"#,
            version = config::CURRENT_VERSION,
        ),
    )
    .unwrap();
}

const MISSING_MODEL_OVERLAY: &str =
    "[routing.routes]\nreviewer = [{ profile = 'local', model = 'missing' }]\n";

/// What a preflight that failed leaves in `out`: the failure named, no replay,
/// the report written with the warning.
fn assert_preflight_evidence(out: &Path, done: &EvaluationJobView) {
    assert!(
        matches!(done.operation.state, OperationState::Failed { .. }),
        "{done:?}"
    );
    let message = done.message.clone().expect("the failure says why");
    assert!(message.contains("evaluation preflight failed"), "{message}");
    assert!(message.contains("missing"), "{message}");
    assert!(done.runs.is_empty(), "no replay ran: {done:?}");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["runs"], serde_json::json!([]));
    assert!(!out.join("runs").exists());
    let report: Value =
        serde_json::from_slice(&std::fs::read(out.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["pairs"], serde_json::json!([]));
    assert!(
        report["warnings"].as_array().unwrap().iter().any(|w| {
            w["kind"] == "preflight_failed" && w["message"].as_str().unwrap().contains("missing")
        }),
        "{report}"
    );
    let markdown = std::fs::read_to_string(out.join("report.md")).unwrap();
    assert!(markdown.contains("no samples compared"), "{markdown}");
    assert!(markdown.contains("missing"), "{markdown}");
}

/// A candidate that names a model the provider does not offer ends the job
/// before any replay or judge request, with the evidence kept in the output
/// directory and the report written (the contract the CLI had when it ran the
/// replay itself).
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_preflight_through_the_core_reports_and_sends_no_inference() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_dir = root.join("config");
    let database = root.join("events.db");
    record_thread(&root, &database);
    let provider = StreamingMockOpenAi::spawn_with_models(
        vec![],
        mock_openai::WriteMode::default(),
        vec!["gpt-4o".into()],
    );
    configure_preflight(&config_dir, &provider.base_url());
    let (mut host, tasks) = job_host(&root, &config_dir, &database);
    let mut events = subscribe(&host);
    let mut start = run_of("preflight", THREAD);
    start.candidate = Some(MISSING_MODEL_OVERLAY.into());
    start.report = true;
    start.output = Some("out".into());
    host.handle_command(
        Command::Evaluation(EvaluationCommand::StartEvaluationRun(Box::new(start))),
        local_desktop(),
    )
    .unwrap();
    let done = settled(&mut events, "preflight").await;
    assert_preflight_evidence(&config_dir.join("eval").join("out"), &done);
    assert!(
        provider
            .recorded_requests()
            .iter()
            .all(|request| request.method == "GET"),
        "no replay or judge inference"
    );
    idle(&tasks).await;
}

/// A failed preflight of a run that was not asked for a report keeps its
/// evidence (the manifest and the failure) and writes no report file.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_preflight_without_a_report_writes_no_report_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_dir = root.join("config");
    let database = root.join("events.db");
    record_thread(&root, &database);
    let provider = StreamingMockOpenAi::spawn_with_models(
        vec![],
        mock_openai::WriteMode::default(),
        vec!["gpt-4o".into()],
    );
    configure_preflight(&config_dir, &provider.base_url());
    let (mut host, tasks) = job_host(&root, &config_dir, &database);
    let mut events = subscribe(&host);
    let mut start = run_of("no-report", THREAD);
    start.candidate = Some(MISSING_MODEL_OVERLAY.into());
    start.report = false;
    start.output = Some("out".into());
    host.handle_command(
        Command::Evaluation(EvaluationCommand::StartEvaluationRun(Box::new(start))),
        local_desktop(),
    )
    .unwrap();
    let done = settled(&mut events, "no-report").await;
    let out = config_dir.join("eval").join("out");
    assert!(
        matches!(done.operation.state, OperationState::Failed { .. }),
        "{done:?}"
    );
    let message = done.message.clone().expect("the failure says why");
    assert!(message.contains("evaluation preflight failed"), "{message}");
    assert!(message.contains("missing"), "{message}");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["runs"], serde_json::json!([]));
    assert!(!out.join("runs").exists());
    assert!(!out.join("report.json").exists());
    assert!(!out.join("report.md").exists());
    assert!(done.reports.is_empty(), "{done:?}");
    idle(&tasks).await;
}

/// A model that never answers and counts the requests it holds: a replay
/// that is stopped must drop them.
struct Hang {
    entered: Arc<tokio::sync::Semaphore>,
    live: Arc<AtomicUsize>,
}

struct Live(Arc<AtomicUsize>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl runtime::AgentModel for Hang {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.live.fetch_add(1, Ordering::SeqCst);
        let _held = Live(self.live.clone());
        self.entered.add_permits(1);
        std::future::pending().await
    }

    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "hang".into()
    }
}

fn hanging(entered: &Arc<tokio::sync::Semaphore>, live: &Arc<AtomicUsize>) -> FixedModels {
    let factory = |entered: Arc<tokio::sync::Semaphore>, live: Arc<AtomicUsize>| {
        Arc::new(move |_: &Arc<event_bus::EventBus>| {
            Arc::new(Hang {
                entered: entered.clone(),
                live: live.clone(),
            }) as Arc<dyn runtime::AgentModel>
        })
    };
    FixedModels {
        agent: factory(entered.clone(), live.clone()),
        simulator: factory(entered.clone(), live.clone()),
    }
}

/// A job whose replay is in its first model request, held by a model that
/// never answers.
struct Replaying {
    host: CommandHost,
    tasks: AreaTasks,
    events: workbench_host::subscriptions::Subscription,
    live: Arc<AtomicUsize>,
    _directory: tempfile::TempDir,
}

async fn replaying(id: &str) -> Replaying {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_dir = root.join("config");
    let database = root.join("events.db");
    record_thread(&root, &database);
    configure(&config_dir, "http://127.0.0.1:9");
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let live = Arc::new(AtomicUsize::new(0));
    let (mut host, tasks) = job_host_with(
        &root,
        &config_dir,
        &database,
        Some(hanging(&entered, &live)),
    );
    let events = subscribe(&host);
    host.handle_command(
        Command::Evaluation(EvaluationCommand::StartEvaluationRun(Box::new(run_of(
            id, THREAD,
        )))),
        local_desktop(),
    )
    .unwrap();
    entered.acquire().await.unwrap().forget();
    assert_eq!(live.load(Ordering::SeqCst), 1, "the replay is mid-request");
    Replaying {
        host,
        tasks,
        events,
        live,
        _directory: directory,
    }
}

fn assert_stopped(done: &EvaluationJobView, reason: CancelReason) {
    assert_eq!(
        done.operation.state,
        OperationState::Cancelled { reason },
        "{done:?}"
    );
    assert!(
        done.runs
            .iter()
            .all(|run| run.status == EvaluationRunStatus::Incomplete),
        "{done:?}"
    );
}

/// A cancel that comes after the replay began stops what it started: the
/// job settles only once the replay's request is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_after_the_replay_began_stops_it_before_the_job_settles() {
    let mut replay = replaying("cancel-running").await;
    replay
        .host
        .handle_command(
            Command::Evaluation(EvaluationCommand::CancelEvaluationJob(
                CancelEvaluationJob {
                    operation_id: operation("cancel-running"),
                },
            )),
            local_desktop(),
        )
        .unwrap();
    let done = settled(&mut replay.events, "cancel-running").await;
    assert_stopped(&done, CancelReason::Requested);
    assert_eq!(
        replay.live.load(Ordering::SeqCst),
        0,
        "the model request was stopped before the job settled"
    );
    idle(&replay.tasks).await;
}

/// The core's shutdown stops a replay in progress the same way.
#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_after_the_replay_began_stops_it_before_the_job_settles() {
    let mut replay = replaying("shutdown-running").await;
    replay.tasks.request_stop();
    let done = settled(&mut replay.events, "shutdown-running").await;
    assert_stopped(&done, CancelReason::Shutdown);
    assert_eq!(replay.live.load(Ordering::SeqCst), 0);
    idle(&replay.tasks).await;
}

/// A report job that fails on its second directory keeps what it finished:
/// the first report is named in the view and the effect is partial.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_job_that_fails_partway_names_the_reports_it_wrote() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_dir = root.join("config");
    let database = root.join("events.db");
    record_thread(&root, &database);
    let provider = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![text("agent-1", "I fixed the bug."), text("user-1", FINISH)],
        vec![MODEL.into()],
    );
    configure(&config_dir, &provider.base_url());
    let (mut host, tasks) = job_host(&root, &config_dir, &database);
    let mut events = subscribe(&host);
    let mut start = run_of("replay", THREAD);
    start.report = true;
    host.handle_command(
        Command::Evaluation(EvaluationCommand::StartEvaluationRun(Box::new(start))),
        local_desktop(),
    )
    .unwrap();
    let done = settled(&mut events, "replay").await;
    assert_eq!(done.operation.state, OperationState::Ready, "{done:?}");
    idle(&tasks).await;

    // A second directory that holds no replay to report.
    std::fs::create_dir(config_dir.join("eval").join("second")).unwrap();

    host.handle_command(
        Command::Evaluation(EvaluationCommand::StartEvaluationReport(
            StartEvaluationReport {
                operation_id: operation("partway"),
                targets: vec!["cache-run".into(), "second".into()],
                rejudge: false,
            },
        )),
        local_desktop(),
    )
    .unwrap();
    let done = settled(&mut events, "partway").await;
    assert!(
        matches!(done.operation.state, OperationState::Failed { .. }),
        "{done:?}"
    );
    assert_eq!(done.reports, vec!["cache-run".to_owned()], "{done:?}");
    assert_eq!(done.effect, EvaluationJobEffect::Partial, "{done:?}");
    idle(&tasks).await;
}

/// The real `evorch-core` process, as the CLI meets it: a candidate the
/// provider cannot serve ends the job in the core before any inference, and
/// the client reads the same settled view from the wire. The core reads the
/// database of its own configuration directory; nothing but the command
/// reaches it from the client.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_core_process_fails_a_preflight_without_inference() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
    use workbench_protocol::handshake::BuildInfo;
    use workbench_protocol::ids::ClientId;

    let dir = test_socket_dir::SplitDir::new();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config_dir = dir.path().canonicalize().unwrap();
    record_thread(&config_dir, &config_dir.join("evorch-events.db"));
    let provider = StreamingMockOpenAi::spawn_with_models(
        vec![],
        mock_openai::WriteMode::default(),
        vec!["gpt-4o".into()],
    );
    configure_preflight(&config_dir, &provider.base_url());
    // The core process has no keyring entry of this test: it reads the key from its environment.
    let config_file = config_dir.join("config.toml");
    let keyring = format!(
        r#"credential = {{ type = "keyring", service = "evorch", account = "{ACCOUNT}" }}"#
    );
    let text = std::fs::read_to_string(&config_file).unwrap();
    assert!(text.contains(&keyring));
    std::fs::write(
        &config_file,
        text.replace(
            &keyring,
            r#"credential = { type = "env", var = "EVAL_CACHE_E2E_KEY" }"#,
        ),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(
        std::env::var_os("CARGO_BIN_EXE_evorch-core").expect("CARGO_BIN_EXE_evorch-core"),
    )
    .args(["--config-dir", config_dir.to_str().unwrap(), "--foreground"])
    .env("XDG_STATE_HOME", dir.sockets().join("state"))
    .env("XDG_RUNTIME_DIR", dir.sockets())
    .env("EVAL_CACHE_E2E_KEY", "fixture-key")
    .current_dir(&config_dir)
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready: Value =
        serde_json::from_str(&lines.next_line().await.unwrap().expect("readiness line")).unwrap();
    assert_eq!(ready["status"], "ready");
    let client = WorkbenchClient::connect(
        UnixTransport::connect(ready["socket"].as_str().unwrap())
            .await
            .unwrap(),
        ClientConfig::new(
            ClientId::new("eval-cache-e2e").unwrap(),
            BuildInfo {
                name: "test".into(),
                version: "1".into(),
                revision: None,
            },
        ),
    )
    .await
    .unwrap();
    let mut events = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .unwrap();

    let mut start = run_of("real-core-preflight", THREAD);
    start.candidate = Some(MISSING_MODEL_OVERLAY.into());
    start.report = true;
    start.output = Some("out".into());
    client
        .command(Command::Evaluation(EvaluationCommand::StartEvaluationRun(
            Box::new(start),
        )))
        .await
        .unwrap();
    let done = loop {
        let envelope = events.next().await.expect("the subscription stays open");
        if let UiEvent::Evaluation(EvaluationEvent::EvaluationJobSettled { view }) = envelope.event
            && view.operation.operation_id.as_str() == "real-core-preflight"
        {
            break *view;
        }
    };
    assert_preflight_evidence(&config_dir.join("eval").join("out"), &done);
    assert!(
        provider
            .recorded_requests()
            .iter()
            .all(|request| request.method == "GET"),
        "no replay or judge inference"
    );
    child.kill().await.unwrap();
}
