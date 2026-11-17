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

use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use sandbox::CredentialStore as _;
use serde_json::Value;
use storage::thread_start::{ThreadStartCapture, ThreadStartSnapshot, ThreadStartState};
use storage::{ReviewEvidenceRow, RunContextRecord, Storage, StorageConfig};
use workbench_host::CommandHost;
use workbench_host::areas::evaluation::{EvaluationArea, Isolation, JobParts, JobSeams};
use workbench_host::areas::{AreaTasks, Areas};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::commands::CommandService;
use workbench_host::projects::StaticProjects;
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::Command;
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::{OperationId, ProjectId, ThreadId};
use workbench_protocol::namespaces::evaluation::{
    EvaluationCommand, EvaluationEvent, EvaluationJobView, EvaluationRunStatus, StartEvaluationRun,
};
use workbench_protocol::operation::OperationState;

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

/// The host of the first scenarios: the production `evaluation` area over the
/// recorded database, with only the isolation of the replays' tools switched off.
fn job_host(root: &Path, config_dir: &Path, database: &Path) -> (CommandHost, AreaTasks) {
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
    assert!(!tasks.busy(), "the job released its counted task");
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
    configure(&config_dir, &provider.base_url());
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
            base_url = provider.base_url(),
        ),
    )
    .unwrap();
    let (mut host, tasks) = job_host(&root, &config_dir, &database);
    let mut events = subscribe(&host);
    let mut start = run_of("preflight", THREAD);
    start.candidate =
        Some("[routing.routes]\nreviewer = [{ profile = 'local', model = 'missing' }]\n".into());
    start.report = true;
    start.output = Some("out".into());
    host.handle_command(
        Command::Evaluation(EvaluationCommand::StartEvaluationRun(Box::new(start))),
        local_desktop(),
    )
    .unwrap();
    let done = settled(&mut events, "preflight").await;
    assert!(
        matches!(done.operation.state, OperationState::Failed { .. }),
        "{done:?}"
    );
    let message = done.message.clone().expect("the failure says why");
    assert!(message.contains("evaluation preflight failed"), "{message}");
    assert!(message.contains("missing"), "{message}");
    assert!(done.runs.is_empty(), "no replay ran: {done:?}");

    let out = config_dir.join("eval").join("out");
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
    assert!(out.join("report.md").exists());
    assert!(
        provider
            .recorded_requests()
            .iter()
            .all(|request| request.method == "GET"),
        "no replay or judge inference"
    );
    assert!(!tasks.busy());
}
