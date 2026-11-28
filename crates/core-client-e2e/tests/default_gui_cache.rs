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
//! * A history read the window makes between two turns (what it does to restore a thread)
//!   changes nothing in the next request.
//! * Closing the window does not stop the core: a second window that connects afterwards
//!   continues the same conversation, and its request extends the earlier ones. This
//!   replaces the embedded window's own close-handling test: closing a window is only the
//!   end of a client connection.
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

/// A core over a real runtime and the offline mock, listening on a Unix socket.
struct Core {
    mock: StreamingMockOpenAi,
    turns: event_bus::EventReceiver,
    identity: core_instance::ConfigIdentity,
    instance: Instance,
    stop: tokio::sync::watch::Sender<bool>,
    socket_task: tokio::task::JoinHandle<()>,
    _dir: test_socket_dir::SplitDir,
    _bridge: OwnedStorageBridge,
}

impl Core {
    /// `skills` are written as repository skills of the one project, `(name, body)`.
    async fn start(responses: Vec<ScriptedResponse>, skills: &[(&str, &str)]) -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
            responses,
            vec!["mock-model".into()],
        );
        for (name, skill_body) in skills {
            let skill = dir.path().join(".evorch/skills").join(name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} procedure\n---\n{skill_body}"),
            )
            .unwrap();
        }
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
        let turns = bus.subscribe();
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
                sandbox::credential::FileCredentialStore::open(dir.path().join("credentials"))
                    .unwrap(),
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

        Self {
            mock,
            turns,
            identity,
            instance,
            stop,
            socket_task,
            _dir: dir,
            _bridge: bridge,
        }
    }

    fn new_launcher(&self) -> core_launcher::Launcher {
        core_launcher::Launcher::with_socket(
            self.identity.clone(),
            core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
            gui::core_client::client_config(),
            self.instance.socket.clone(),
        )
        .unwrap()
    }

    fn finish(self) {
        self.stop.send(true).unwrap();
        self.socket_task.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_typed_in_the_default_window_keeps_the_model_prefix() {
    let mut core = Core::start(
        vec![
            ScriptedResponse::text_stream("first", "mock-model", ["hello"]),
            ScriptedResponse::text_stream("second", "mock-model", ["continued"]),
            ScriptedResponse::text_stream("third", "mock-model", ["again"]),
            ScriptedResponse::text_stream("fourth", "mock-model", ["from a new window"]),
        ],
        &[],
    )
    .await;
    let launcher = core.new_launcher();
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
    turn_completed(&mut core.turns).await;
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
    turn_completed(&mut core.turns).await;

    // What the window reads to restore a thread, between two turns.
    actions
        .send(Action::History(
            ThreadId::new("thread").unwrap(),
            None,
            false,
        ))
        .await
        .unwrap();
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::SendChat(chat(
                "three",
                "third prompt",
            ))),
            draft: None,
        })
        .await
        .unwrap();
    turn_completed(&mut core.turns).await;

    // The window is closed. The core, its runtime and the conversation stay; a new window
    // connects and continues the same thread.
    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let window = tokio::spawn(serve(
        core.new_launcher(),
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    connected(&shared).await;
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::SendChat(chat(
                "four",
                "fourth prompt",
            ))),
            draft: None,
        })
        .await
        .unwrap();
    turn_completed(&mut core.turns).await;

    let requests: Vec<_> = core
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .collect();
    assert_eq!(requests.len(), 4, "one provider request per turn");
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0].body, &pair[1].body).unwrap();
        // Each request is longer than the one before: the check above is about a prefix,
        // not about two equal bodies.
        assert!(pair[1].body.to_string().len() > pair[0].body.to_string().len());
    }

    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");
    core.finish();
}

fn user_texts(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| match &message["content"] {
            serde_json::Value::String(text) => text.clone(),
            parts => parts
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect(),
        })
        .collect()
}

const SKILL_BODY: &str = "Step one of the demo.\nStep two of the demo.\n";
const SKILL_BLOCK: &str = "<skill name=\"demo\">";

/// What the composer does with the skills of a project: asks the core for the list
/// through the client actor, and waits until the workspace feature holds it.
async fn skills_listed(
    actions: &tokio::sync::mpsc::Sender<Action>,
    shared: &Arc<Mutex<Shared>>,
) -> Vec<String> {
    let project = ProjectId::new("project").unwrap();
    let changed = shared.lock().unwrap().changed.clone();
    let (tag, query) = {
        let mut state = shared.lock().unwrap();
        let workspace = state.features.workspace_mut().unwrap();
        workspace.refresh_skills(&project);
        match workspace.take_outbox().pop().unwrap() {
            gui::core_client::features::workspace::Outgoing::Query(tag, query) => (tag, query),
            other => panic!("a query expected, got {other:?}"),
        }
    };
    actions
        .send(Action::Feature(gui::core_client::features::FeatureAction {
            feature: gui::core_client::features::FeatureId::Workspace,
            tag,
            request: gui::core_client::features::FeatureRequest::Query(query),
        }))
        .await
        .unwrap();
    async {
        loop {
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            let listed = {
                let state = shared.lock().unwrap();
                let workspace = state.features.workspace().unwrap();
                // The answer of this request, not a list held from an earlier one.
                match workspace
                    .skills("project")
                    .filter(|_| !workspace.skills_fetching("project"))
                {
                    Some(gui::core_client::features::workspace::ProjectSkills::Ready(list)) => {
                        Some(list.iter().map(|skill| skill.name.clone()).collect())
                    }
                    Some(gui::core_client::features::workspace::ProjectSkills::Failed) => {
                        panic!("the core could not list the skills")
                    }
                    None => None,
                }
            };
            if let Some(listed) = listed {
                return listed;
            }
            notification.await;
        }
    }
    .await
}

// The skill a message mentions is read by the core into that message only. The client
// sends the skill's name; the body joins the new user input of each turn that names it,
// and nothing sent before changes: system, tools, model settings and every earlier
// message stay as they were, so the cache prefix keeps growing. Listing the skills
// between two turns asks the model nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_skill_mentioned_in_the_default_window_joins_only_its_own_turn() {
    let mut core = Core::start(
        vec![
            ScriptedResponse::text_stream("first", "mock-model", ["hello"]),
            ScriptedResponse::text_stream("second", "mock-model", ["continued"]),
            ScriptedResponse::text_stream("third", "mock-model", ["again"]),
        ],
        &[("demo", SKILL_BODY)],
    )
    .await;
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let window = tokio::spawn(serve(
        core.new_launcher(),
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    connected(&shared).await;

    // The composer's list is what the core reads skills from: `demo` is in it, so a
    // message that mentions `@demo` names it.
    assert!(
        skills_listed(&actions, &shared)
            .await
            .contains(&"demo".to_owned())
    );
    let named = |text: &str| -> Vec<String> {
        let listed = ["demo".to_owned()];
        text.split_whitespace()
            .filter_map(|word| word.strip_prefix('@'))
            .filter(|name| listed.iter().any(|skill| skill == name))
            .map(str::to_owned)
            .collect()
    };
    let with_skill = |submission: &str, text: &str| {
        let mut send = chat(submission, text);
        send.skills = named(text);
        send
    };

    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::CreateExecution(with_skill(
                "one",
                "first with @demo",
            ))),
            draft: None,
        })
        .await
        .unwrap();
    turn_completed(&mut core.turns).await;
    // Listing again between the turns, as a mention starting does.
    assert!(
        skills_listed(&actions, &shared)
            .await
            .contains(&"demo".to_owned())
    );
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::SendChat(with_skill(
                "two",
                "second with @demo",
            ))),
            draft: None,
        })
        .await
        .unwrap();
    turn_completed(&mut core.turns).await;
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::SendChat(with_skill(
                "three",
                "third plain",
            ))),
            draft: None,
        })
        .await
        .unwrap();
    turn_completed(&mut core.turns).await;

    let requests: Vec<_> = core
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .collect();
    assert_eq!(
        requests.len(),
        3,
        "one provider request per turn, none for the lists"
    );
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0].body, &pair[1].body).unwrap();
        assert!(pair[1].body.to_string().len() > pair[0].body.to_string().len());
    }
    let users: Vec<_> = requests.iter().map(|r| user_texts(&r.body)).collect();
    // Each turn adds exactly one user message, and only a turn that named the skill
    // carries its body.
    assert_eq!(users[0].len(), 1);
    assert_eq!(users[1].len(), 2);
    assert_eq!(users[2].len(), 3);
    assert_eq!(users[1][..1], users[0][..]);
    assert_eq!(users[2][..2], users[1][..]);
    let carries = |text: &str| text.matches(SKILL_BLOCK).count();
    assert_eq!(
        users
            .iter()
            .map(|turns| turns.iter().map(|text| carries(text)).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [vec![1], vec![1, 1], vec![1, 1, 0]]
    );
    for turn in [&users[0][0], &users[1][1]] {
        assert!(turn.contains(SKILL_BODY.trim_end()), "{turn}");
    }
    assert!(users[2][2].starts_with("third plain") && !users[2][2].contains(SKILL_BLOCK));

    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");
    core.finish();
}
