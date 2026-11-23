//! The default desktop (stage 19a) is a client of the core, so a conversation the user
//! types in the window reaches the model through the core's one runtime. This is the
//! prompt-cache contract of that path: the real client actor (`gui::core_client`, with the
//! client identity the default window uses) sends a new conversation and a follow-up over
//! the core's Unix socket, the core calls the production routing and HTTP adapters, and the
//! requests are read back at the HTTP boundary of the offline mock, whose prompt cache is
//! derived from the request bytes.
//!
//! * The follow-up's request extends the first one (same model, cache key, tool schema and
//!   prefix): what the client sends, and the connection it uses, add nothing to what the
//!   model is sent.
//!
//! Every wait is for an event of the core's bus or of the client's state; the timeouts only
//! bound a failing test.
use gui::core_client::{
    Shared,
    connection::{Action, serve},
};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use event_bus::{EventBus, EventKind};
use evorch_core::{backend::HostBackend, instance::Instance, unix};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
use workbench_host::areas::{Areas, execution::ExecutionArea};
use workbench_host::{CommandHost, commands::CommandService, projects::StaticProjects};
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::ids::{ProjectId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::ConversationCommand;

const WAIT: Duration = Duration::from_secs(60);

fn chat(submission: &str, text: &str) -> SendChat {
    SendChat {
        project: ProjectId::new("project").unwrap(),
        thread: ThreadId::new("thread").unwrap(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Worker,
        text: text.into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }
}

async fn turn_completed(events: &mut event_bus::EventReceiver) {
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted { .. }) => return,
                EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: event_bus::AgentRunPhase::Error,
                    reason,
                    ..
                }) => panic!("run failed: {reason:?}"),
                _ => (),
            }
        }
    })
    .await
    .expect("a turn completes");
}

async fn connected(shared: &Arc<Mutex<Shared>>) {
    let changed = shared.lock().unwrap().changed.clone();
    tokio::time::timeout(WAIT, async {
        loop {
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            if shared.lock().unwrap().connected {
                return;
            }
            notification.await;
        }
    })
    .await
    .expect("the client connects to the core");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_typed_in_the_default_window_keeps_the_model_prefix() {
    let dir = test_socket_dir::SplitDir::new();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::text_stream("first", "mock-model", ["hello"]),
            ScriptedResponse::text_stream("second", "mock-model", ["continued"]),
        ],
        vec!["mock-model".into()],
    );
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "EVORCH_TEST_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.worker]]
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
    let mut turns = bus.subscribe();
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
        |h| StorageBridge::new(h, "default-gui-cache").with_usage_ledger(pricing),
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
    let service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
        .with_event_bus(bus.clone())
        .with_team_writer(bridge.handle());
    let mut projects = StaticProjects::default();
    projects.insert(&ProjectId::new("project").unwrap(), dir.path().into());
    let areas = Areas {
        execution: ExecutionArea::with_runtime(runtime.clone()),
        ..Areas::default()
    };
    let host = Arc::new(Mutex::new(
        CommandHost::new(service)
            .with_projects(projects)
            .with_areas(areas),
    ));
    let identity = core_instance::ConfigIdentity::new(&dir.path().join("config")).unwrap();
    let instance = Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.sockets(),
    )
    .unwrap();
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let backend = Arc::new(HostBackend::new(host, instance.identity.clone()));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let socket_task = tokio::spawn(async move {
        let mut connections = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            connections.push(tokio::spawn(unix::connection(
                stream,
                backend.clone(),
                stopped.clone(),
            )));
        }
    });
    let launcher = core_launcher::Launcher::with_socket(
        identity.clone(),
        core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
        gui::core_client::client_config(),
        instance.socket.clone(),
    )
    .unwrap();
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let window = tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    connected(&shared).await;

    // A new conversation, then a follow-up in the same thread, as the window sends them.
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::CreateExecution(chat(
                "one",
                "first prompt",
            ))),
            draft: None,
        })
        .await
        .unwrap();
    turn_completed(&mut turns).await;
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::SendChat(chat(
                "two",
                "second prompt",
            ))),
            draft: None,
        })
        .await
        .unwrap();
    turn_completed(&mut turns).await;

    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .collect();
    assert_eq!(requests.len(), 2, "one provider request per turn");
    assert_append_only(CacheProtocol::OpenAi, &requests[0].body, &requests[1].body).unwrap();
    // The follow-up is longer than the first request: the check above is about a prefix,
    // not about two equal bodies.
    assert!(requests[1].body.to_string().len() > requests[0].body.to_string().len());

    actions.send(Action::Shutdown).await.unwrap();
    let _ = tokio::time::timeout(WAIT, window).await;
    stop.send(true).unwrap();
    socket_task.abort();
}
