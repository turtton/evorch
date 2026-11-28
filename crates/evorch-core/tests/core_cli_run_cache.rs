//! `evorch run` through the core keeps the prompt-cache contract (ADR 0045 §10,
//! stage 17): a CLI invocation is a client of the core's one runtime, so its run
//! crosses the production routing and HTTP adapters, and its requests are read
//! back at the HTTP boundary of the offline mock, whose prompt cache is derived
//! from the request bytes.
//!
//! * The run asks a blocking question, ends its turn while the question is
//!   open, the command line answers it, and the run goes on: each request
//!   extends the one before it (same model, cache key, tool schema and
//!   prefix), and starting, following and answering add nothing to what the
//!   model is sent.
//!
//! The answer is sent only once the run waits for it. An answer that arrives
//! while a model turn is in flight is, by design, work for one more turn, so
//! sending it as soon as the question is visible would race the run's second
//! request (the model's script has no turn for that).
//!
//! No test waits for time: every wait is for an event of the core's bus.
use std::sync::{Arc, RwLock};
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, WaitingCause};
use evorch_core::{backend::HostBackend, instance::Instance, unix};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_host::areas::{Areas, execution::ExecutionArea};
use workbench_host::{CommandHost, commands::CommandService, projects::StaticProjects};
use workbench_protocol::commands::{AnswerUserQuestion, Command, CommandResult};
use workbench_protocol::handshake::{BuildInfo, LocalSurface};
use workbench_protocol::ids::{ClientId, InvocationId, ProjectId};
use workbench_protocol::namespaces::execution::invocation::{
    GetInvocation, InvocationRole, InvocationState, StartInvocation,
};
use workbench_protocol::namespaces::{
    ExecutionCommand, ExecutionQuery, ExecutionReply, ExecutionResult, QuestionsCommand,
    QuestionsQuery, QuestionsReply,
};
use workbench_protocol::queries::{PendingQuery, Query, QueryReply};

const MODEL: &str = "mock-model";

#[tokio::test(flavor = "multi_thread")]
async fn a_cli_invocation_keeps_the_sent_prefix_across_its_question_and_answer() {
    let dir = test_socket_dir::SplitDir::new();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::tool_call(
                "ask",
                MODEL,
                0,
                "call-ask",
                "ask_user",
                [r#"{"title":"Which format?","options":["JSON","YAML"],"blocking":true}"#],
            ),
            // The turn ends with the blocking question open, so the run waits
            // for the answer; the answer then starts the last turn.
            ScriptedResponse::text_stream("wait", MODEL, ["waiting for the answer"]),
            ScriptedResponse::text_stream("done", MODEL, ["all done"]),
        ],
        vec![MODEL.into()],
    );
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "EVORCH_TEST_KEY"
models = ["{MODEL}"]
default_model = "{MODEL}"
[[routing.routes.orchestrator]]
profile = "local"
[compaction]
context_window_tokens = 1000000
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let config = config::Config::load(&config::LoadOptions {
        user_config_dir: Some(dir.path().into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let mut events = bus.subscribe();
    let storage_config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(storage_config.clone()).unwrap();
    let pricing = Arc::new(RwLock::new(recorder::pricing::UsagePricing::new(
        config.providers.clone(),
        None,
    )));
    let bridge = OwnedStorageBridge::spawn(
        bus.clone(),
        store,
        |h| StorageBridge::new(h, "core-test").with_usage_ledger(pricing),
        Duration::from_secs(3600),
    )
    .unwrap();
    let runtime = runtime::compose_runtime(runtime::RuntimeComposition {
        config: &config,
        bus: bus.clone(),
        executor: Arc::new(tools::ToolExecutor::new(bus.clone())),
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path().join("credentials")).unwrap(),
        ),
        env: Arc::new(routing::MapEnv::from_iter([(
            "EVORCH_TEST_KEY",
            "offline-test-key",
        )])),
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(dir.path().into()),
        model_source: runtime::ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_run_store(runtime::RunStore::open(&storage_config, bridge.handle()).unwrap());
    let mut sidebar = workspace_state::SidebarState::default();
    sidebar
        .add_project(
            workspace_state::ProjectId::new("project"),
            "cache-test",
            dir.path(),
        )
        .unwrap();
    sidebar.projects[0].trust = workspace_state::TrustState::Approved;
    let sidebar_path = dir.path().join("sidebar.json");
    std::fs::write(&sidebar_path, serde_json::to_vec(&sidebar).unwrap()).unwrap();
    let service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
        .with_event_bus(bus.clone())
        .with_team_writer(bridge.handle())
        .with_sidebar_path(sidebar_path);
    let mut projects = StaticProjects::default();
    projects.insert(&ProjectId::new("project").unwrap(), dir.path().into());
    let host = Arc::new(std::sync::Mutex::new(
        CommandHost::new(service)
            .with_projects(projects)
            .with_areas(Areas {
                execution: ExecutionArea::with_runtime(runtime.clone()),
                ..Areas::default()
            }),
    ));
    let instance = Instance::acquire(
        &evorch_core::instance::LaunchGuard::acquire(
            &evorch_core::instance::ConfigIdentity::new(&dir.path().join("config")).unwrap(),
        )
        .unwrap(),
        dir.sockets(),
    )
    .unwrap();
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let backend = Arc::new(HostBackend::new(host, instance.identity.clone()));
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let socket_backend = backend.clone();
    let _socket_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        unix::connection(stream, socket_backend, stopped)
            .await
            .unwrap();
    });

    let mut client_config = ClientConfig::new(
        ClientId::new("cli-cache-test").unwrap(),
        BuildInfo {
            name: "test".into(),
            version: "1".into(),
            revision: None,
        },
    );
    client_config.local_surface = Some(LocalSurface::Cli);
    let client = WorkbenchClient::connect(
        UnixTransport::connect(&instance.socket).await.unwrap(),
        client_config,
    )
    .await
    .unwrap();
    let invocation = InvocationId::new("run-cache-1").unwrap();
    let reply = client
        .command(Command::Execution(ExecutionCommand::StartInvocation(
            StartInvocation {
                invocation: invocation.clone(),
                project_root: dir.path().to_string_lossy().into_owned(),
                role: InvocationRole::Orchestrator,
                prompt: "pick a format".into(),
                thread: None,
            },
        )))
        .await
        .unwrap();
    let Some(CommandResult::Execution(ExecutionResult::InvocationStarted(started))) =
        reply.result.as_deref()
    else {
        panic!("not an invocation start: {reply:?}")
    };
    let thread = started.thread.clone();

    // The second turn ends with the blocking question open, so the run waits
    // for the answer. Only then is the answer sent: the bus was subscribed
    // before the run started, so this event cannot be missed.
    loop {
        let event = events.recv().await.expect("the bus is open");
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            to: AgentRunPhase::Waiting,
            waiting: Some(WaitingCause::BlockingQuestions),
            ..
        }) = event.kind
        {
            break;
        }
    }
    let QueryReply::Questions(QuestionsReply::Questions(page)) = client
        .query(Query::Questions(QuestionsQuery::ListQuestions(
            PendingQuery {
                thread: Some(thread.clone()),
                pending_only: true,
                page: Default::default(),
            },
        )))
        .await
        .unwrap()
    else {
        panic!("questions reply")
    };
    let question = page
        .items
        .into_iter()
        .next()
        .expect("the waiting run has its question open");
    assert_eq!(question.title, "Which format?");
    client
        .command(Command::Questions(QuestionsCommand::AnswerUserQuestion(
            AnswerUserQuestion {
                thread: thread.clone(),
                question_id: question.id.clone(),
                answer: "YAML".into(),
                question_revision: question.revision,
            },
        )))
        .await
        .unwrap();

    let finished = loop {
        let QueryReply::Execution(ExecutionReply::Invocation(view)) = client
            .query(Query::Execution(ExecutionQuery::GetInvocation(
                GetInvocation {
                    invocation: invocation.clone(),
                },
            )))
            .await
            .unwrap()
        else {
            panic!("invocation reply")
        };
        if view.state.is_terminal() {
            break view;
        }
        loop {
            if let EventKind::Lifecycle(_) = events.recv().await.unwrap().kind {
                break;
            }
        }
    };
    assert_eq!(finished.state, InvocationState::Succeeded, "{finished:?}");

    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect();
    assert_eq!(
        requests.len(),
        3,
        "one request per model turn: the question, the wait, the answer"
    );
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1])
            .expect("the answered run keeps the sent prefix and the settings");
        for field in ["model", "prompt_cache_key", "tools"] {
            assert_eq!(pair[0][field], pair[1][field], "{field}");
        }
        assert!(
            pair[1]["messages"].as_array().unwrap().len()
                > pair[0]["messages"].as_array().unwrap().len(),
            "each request only appends"
        );
    }
    assert_eq!(
        mock.remaining_scripts(),
        0,
        "every scripted answer was used"
    );
}
