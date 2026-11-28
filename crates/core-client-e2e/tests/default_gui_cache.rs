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
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_host::areas::{AreaTasks, Areas, execution::ExecutionArea};
use workbench_host::{CommandHost, commands::CommandService, projects::StaticProjects};
use workbench_protocol::commands::{
    Command, CommandDisposition, CommandResult, ComposerRole, SendChat,
};
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::handshake::BuildInfo;
use workbench_protocol::ids::{ClientId, OperationId, ProjectId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::execution::{AskBtw, GetBtw};
use workbench_protocol::namespaces::{
    ConversationCommand, ExecutionCommand, ExecutionEvent, ExecutionQuery, ExecutionReply,
    ExecutionResult,
};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{Query, QueryReply};

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
        Self::boot(dir, mock).await
    }

    /// A planned restart of the core: the listening process goes away and a new one starts over
    /// the same storage, config and provider. What it knows of the inputs is what it reads.
    async fn restart(self) -> Self {
        let Self {
            mock,
            stop,
            socket_task,
            instance,
            _dir,
            _bridge,
            ..
        } = self;
        stop.send(true).unwrap();
        socket_task.abort();
        let _ = socket_task.await;
        drop(instance);
        drop(_bridge);
        Self::boot(_dir, mock).await
    }

    /// The core's parts over `dir`, which holds its config and database.
    async fn boot(dir: test_socket_dir::SplitDir, mock: StreamingMockOpenAi) -> Self {
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
        runtime.set_thread_scratch(Arc::new(runtime::scratch::ThreadScratch::at(
            dir.path().join("scratch"),
        )));
        let service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
            .with_event_bus(bus.clone())
            .with_team_writer(bridge.handle());
        let mut projects = StaticProjects::default();
        projects.insert(&ProjectId::new("project").unwrap(), dir.path().into());
        let host = CommandHost::new(service).with_projects(projects);
        let areas = Areas {
            execution: ExecutionArea::with_runtime(runtime.clone()).with_side_questions(
                AreaTasks::new(tokio::runtime::Handle::current()),
                host.projection().source("execution").unwrap(),
            ),
            ..Areas::default()
        };
        let host = Arc::new(Mutex::new(host.with_areas(areas)));
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

/// Waits for the turn to complete and returns `(input_tokens, cache_read_tokens)` of the
/// provider requests it made, as the core observed them. The wait is for the turn's own
/// terminal event, not for a duration; a hang is the runner's to cut off.
async fn turn_usage(events: &mut event_bus::EventReceiver) -> Vec<(u64, u64)> {
    let mut usage = Vec::new();
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Provider(event_bus::ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => usage.push((input_tokens, cache_read_tokens)),
            EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted { .. }) => return usage,
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                to: event_bus::AgentRunPhase::Error,
                reason,
                ..
            }) => panic!("run failed: {reason:?}"),
            _ => (),
        }
    }
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
    let mut usage = Vec::new();
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
    usage.extend(turn_usage(&mut core.turns).await);
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
    usage.extend(turn_usage(&mut core.turns).await);
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
    usage.extend(turn_usage(&mut core.turns).await);

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
    // The mock derives its cache from each request's own bytes: what the turn before sent
    // is read back as cache by the next, though the next adds a skill block.
    assert_eq!(usage.len(), 3);
    assert!(usage[0].0 > 0);
    for pair in usage.windows(2) {
        assert!(pair[1].1 >= pair[0].0, "{usage:?}");
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

    // The actor ends on Shutdown; its end is what is awaited, not a duration.
    actions.send(Action::Shutdown).await.unwrap();
    window.await.expect("the actor does not panic");
    core.finish();
}

const SIDE_QUESTION: &str = "side question: which file did you read?";
const SIDE_ANSWER: &str = "side answer: none yet";

fn side_texts(body: &serde_json::Value) -> String {
    body["messages"].to_string()
}

// A side question asked through the core between two turns of the window's conversation:
// the request it makes is the first turn's context plus the question (tools kept, only
// tool calls forbidden), and neither the question nor its answer reaches the second
// turn, which extends the first and reads its whole input back as cache.
#[tokio::test(flavor = "multi_thread")]
async fn a_side_question_between_two_turns_leaves_the_model_prefix_alone() {
    let mut core = Core::start(
        vec![
            ScriptedResponse::text_stream("first", "mock-model", ["hello"]),
            ScriptedResponse::text_stream("side", "mock-model", [SIDE_ANSWER]),
            ScriptedResponse::text_stream("second", "mock-model", ["continued"]),
        ],
        &[],
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
    let first_usage = turn_usage(&mut core.turns).await;

    // What the window's composer sends as `/btw`, from a client of its own so that the
    // wait is for the operation's change event.
    let client = WorkbenchClient::connect(
        UnixTransport::connect(&core.instance.socket).await.unwrap(),
        ClientConfig::new(
            ClientId::new("btw-test").unwrap(),
            BuildInfo {
                name: "test".into(),
                version: "1".into(),
                revision: None,
            },
        ),
    )
    .await
    .unwrap();
    let mut subscription = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .unwrap();
    let operation = OperationId::new("btw-1").unwrap();
    let reply = client
        .command(Command::Execution(ExecutionCommand::AskBtw(AskBtw {
            operation_id: operation.clone(),
            thread: ThreadId::new("thread").unwrap(),
            question: SIDE_QUESTION.into(),
        })))
        .await
        .unwrap();
    let Some(CommandResult::Execution(ExecutionResult::Btw(accepted))) = reply.result.as_deref()
    else {
        panic!("the accepted operation: {reply:?}");
    };
    assert_eq!(accepted.operation_id, operation);
    let mut announced = vec![serde_json::to_string(&reply).unwrap()];
    loop {
        let event = subscription
            .next()
            .await
            .expect("the subscription stays open");
        if let UiEvent::Execution(ExecutionEvent::BtwChanged {
            operation: changed, ..
        }) = &event.event
            && changed.operation_id == operation
            && changed.state.is_final()
        {
            announced.push(serde_json::to_string(&event).unwrap());
            break;
        }
    }
    let QueryReply::Execution(ExecutionReply::Btw(view)) = client
        .query(Query::Execution(ExecutionQuery::GetBtw(GetBtw {
            operation_id: operation.clone(),
        })))
        .await
        .unwrap()
    else {
        panic!("the side question's view");
    };
    assert_eq!(view.operation.state, OperationState::Ready);
    assert_eq!(view.answer.as_deref(), Some(SIDE_ANSWER));
    assert!(view.source.as_ref().is_some_and(|source| source.settled));
    for text in announced {
        assert!(
            !text.contains(SIDE_ANSWER) && !text.contains(SIDE_QUESTION),
            "{text}"
        );
    }

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
    let later_usage = turn_usage(&mut core.turns).await;

    let requests: Vec<_> = core
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .map(|r| r.body)
        .collect();
    assert_eq!(requests.len(), 3, "first turn, side question, second turn");
    let (first, side, second) = (&requests[0], &requests[1], &requests[2]);
    assert_append_only(CacheProtocol::OpenAi, first, second).unwrap();
    let side_messages = side["messages"].as_array().unwrap();
    let first_messages = first["messages"].as_array().unwrap();
    assert_eq!(side_messages[..first_messages.len()], first_messages[..]);
    assert!(
        side_messages
            .last()
            .unwrap()
            .to_string()
            .contains(SIDE_QUESTION)
    );
    for key in ["model", "tools", "prompt_cache_key"] {
        assert_eq!(side.get(key), first.get(key), "{key}");
        assert_eq!(second.get(key), first.get(key), "{key}");
    }
    assert_eq!(side["tool_choice"], "none");
    assert_ne!(second.get("tool_choice"), Some(&serde_json::json!("none")));
    for request in [first, second] {
        let text = side_texts(request);
        assert!(
            !text.contains(SIDE_QUESTION) && !text.contains(SIDE_ANSWER),
            "{text}"
        );
    }
    // The mock's cache is derived from each request's own bytes: the second turn reads the
    // first turn's whole input back although the side request came in between.
    let (first_input, _) = *first_usage.last().unwrap();
    let (_, second_cached) = *later_usage.last().unwrap();
    assert!(
        second_cached >= first_input,
        "{first_usage:?} {later_usage:?}"
    );

    actions.send(Action::Shutdown).await.unwrap();
    window.await.expect("the actor does not panic");
    core.finish();
}

struct ReleaseOnDrop(mock_openai::ResponseGate);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn ask_btw(operation: &OperationId, question: &str) -> Command {
    Command::Execution(ExecutionCommand::AskBtw(AskBtw {
        operation_id: operation.clone(),
        thread: ThreadId::new("thread").unwrap(),
        question: question.into(),
    }))
}

// A side question held at the provider: the core keeps answering other requests meanwhile,
// the same id with the same request joins it while another request under the id is a
// conflict, and a cancel settles it as requested without an answer. The provider's reply
// is held by a gate the test opens, and every wait is for the gate's arrival or the
// operation's own change event.
#[tokio::test(flavor = "multi_thread")]
async fn a_side_question_held_at_the_provider_joins_conflicts_and_cancels() {
    let (gate, arrived) = mock_openai::ResponseGate::new();
    let release = ReleaseOnDrop(gate.clone());
    let mut core = Core::start(
        vec![
            ScriptedResponse::text_stream("first", "mock-model", ["hello"]),
            ScriptedResponse::text_stream("side", "mock-model", [SIDE_ANSWER]).with_gate(gate),
        ],
        &[],
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
    turn_usage(&mut core.turns).await;

    let client = WorkbenchClient::connect(
        UnixTransport::connect(&core.instance.socket).await.unwrap(),
        ClientConfig::new(
            ClientId::new("btw-held").unwrap(),
            BuildInfo {
                name: "test".into(),
                version: "1".into(),
                revision: None,
            },
        ),
    )
    .await
    .unwrap();
    let mut subscription = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .unwrap();
    let operation = OperationId::new("btw-held-1").unwrap();
    let first = client
        .command(ask_btw(&operation, SIDE_QUESTION))
        .await
        .unwrap();
    assert_eq!(first.disposition, CommandDisposition::Accepted);
    // The request has reached the provider and waits at the gate.
    tokio::task::spawn_blocking(move || arrived.recv().unwrap())
        .await
        .unwrap();

    // The same request joins the operation; another request under the id is refused.
    let joined = client
        .command(ask_btw(&operation, SIDE_QUESTION))
        .await
        .unwrap();
    assert_eq!(joined.disposition, CommandDisposition::AlreadyApplied);
    let Err(workbench_client::ClientError::Server(refused)) = client
        .command(ask_btw(&operation, "another question"))
        .await
    else {
        panic!("a different request under the id is refused");
    };
    assert_eq!(
        refused.code,
        workbench_protocol::errors::ErrorCode::Conflict
    );

    // While the provider holds the answer, the core answers other requests and shows the
    // operation as pending without an answer.
    let QueryReply::Execution(ExecutionReply::Btw(pending)) = client
        .query(Query::Execution(ExecutionQuery::GetBtw(GetBtw {
            operation_id: operation.clone(),
        })))
        .await
        .unwrap()
    else {
        panic!("the side question's view");
    };
    assert_eq!(pending.operation.state, OperationState::Pending);
    assert!(pending.answer.is_none());
    client
        .query(Query::Execution(ExecutionQuery::GetThreadExecution(
            workbench_protocol::commands::ThreadTarget {
                thread: ThreadId::new("thread").unwrap(),
            },
        )))
        .await
        .unwrap();

    // A cancel settles it as requested; asking it to cancel again changes nothing.
    client
        .command(Command::Execution(ExecutionCommand::CancelBtw(
            workbench_protocol::namespaces::execution::CancelBtw {
                operation_id: operation.clone(),
            },
        )))
        .await
        .unwrap();
    loop {
        let event = subscription
            .next()
            .await
            .expect("the subscription stays open");
        if let UiEvent::Execution(ExecutionEvent::BtwChanged {
            operation: changed, ..
        }) = &event.event
            && changed.operation_id == operation
            && changed.state.is_final()
        {
            break;
        }
    }
    let QueryReply::Execution(ExecutionReply::Btw(settled)) = client
        .query(Query::Execution(ExecutionQuery::GetBtw(GetBtw {
            operation_id: operation.clone(),
        })))
        .await
        .unwrap()
    else {
        panic!("the side question's view");
    };
    assert_eq!(
        settled.operation.state,
        OperationState::Cancelled {
            reason: workbench_protocol::operation::CancelReason::Requested
        }
    );
    assert!(settled.answer.is_none());
    let again = client
        .command(Command::Execution(ExecutionCommand::CancelBtw(
            workbench_protocol::namespaces::execution::CancelBtw {
                operation_id: operation.clone(),
            },
        )))
        .await
        .unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);

    // The mock joins its worker threads when it is dropped, so the held response goes first.
    drop(release);

    actions.send(Action::Shutdown).await.unwrap();
    window.await.expect("the actor does not panic");
    core.finish();
}

// Edit and Restore of queued follow-ups (ADR 0045 E7) over the real core, socket and client
// actor. A follow-up is queued while the first turn is held at the provider; the window is
// then rebuilt, and everything it needs to edit or restore the input comes from the core.

/// An actor over a new connection, with the state it keeps. Dropped with `Action::Shutdown`.
async fn window(
    core: &Core,
) -> (
    tokio::sync::mpsc::Sender<Action>,
    Arc<Mutex<Shared>>,
    tokio::task::JoinHandle<()>,
) {
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let task = tokio::spawn(serve(
        core.new_launcher(),
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    connected(&shared).await;
    (actions, shared, task)
}

/// Waits for the client's state to satisfy `ready`; the wait is for its change notices.
async fn until(shared: &Arc<Mutex<Shared>>, what: &str, ready: impl Fn(&Shared) -> bool) {
    let changed = shared.lock().unwrap().changed.clone();
    tokio::time::timeout(WAIT, async {
        loop {
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            if ready(&shared.lock().unwrap()) {
                return;
            }
            notification.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the client's state never showed {what}"));
}

fn receipt_in(
    shared: &Shared,
    id: &str,
    state: workbench_protocol::views::InputReceiptState,
) -> Option<workbench_protocol::views::InputReceipt> {
    shared
        .cache
        .receipts
        .get(id)
        .filter(|receipt| receipt.state == state)
        .cloned()
}

async fn protocol_client(core: &Core, name: &str) -> WorkbenchClient {
    let mut config = ClientConfig::new(
        ClientId::new(name).unwrap(),
        BuildInfo {
            name: "test".into(),
            version: "1".into(),
            revision: None,
        },
    );
    // Uploads are conversation commands, which a client asks for explicitly.
    config
        .optional_capabilities
        .push(workbench_protocol::handshake::Capability::Conversation);
    WorkbenchClient::connect(
        UnixTransport::connect(&core.instance.socket).await.unwrap(),
        config,
    )
    .await
    .unwrap()
}

async fn read_draft(
    client: &WorkbenchClient,
    submission: &str,
) -> workbench_protocol::namespaces::conversation::InputDraft {
    let QueryReply::Conversation(workbench_protocol::namespaces::ConversationReply::InputDraft(
        draft,
    )) = client
        .query(Query::Conversation(
            workbench_protocol::namespaces::ConversationQuery::GetInputDraft(
                workbench_protocol::namespaces::conversation::GetInputDraft {
                    thread: ThreadId::new("thread").unwrap(),
                    submission_id: SubmissionId::new(submission).unwrap(),
                },
            ),
        ))
        .await
        .unwrap()
    else {
        panic!("the words of the input");
    };
    draft
}

/// Uploads a small file of `media_type` to the thread and returns its reference id.
async fn upload(
    client: &WorkbenchClient,
    name: &str,
    media_type: &str,
) -> workbench_protocol::ids::AttachmentId {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let result = |reply: workbench_protocol::commands::CommandReply| match reply.result.map(|r| *r)
    {
        Some(CommandResult::Conversation(result)) => result,
        other => panic!("expected a conversation result, got {other:?}"),
    };
    let begun = client
        .command(Command::Conversation(
            ConversationCommand::BeginAttachment {
                project: ProjectId::new("project").unwrap(),
                thread: ThreadId::new("thread").unwrap(),
                client_upload_id: format!("upload-{name}"),
                filename: name.into(),
                media_type_hint: media_type.into(),
                byte_length: 4,
            },
        ))
        .await
        .unwrap();
    let workbench_protocol::conversation::ConversationResult::AttachmentBegun { upload_id, .. } =
        result(begun)
    else {
        panic!("the upload begins");
    };
    client
        .command(Command::Conversation(
            ConversationCommand::AppendAttachment {
                upload_id: upload_id.clone(),
                offset: 0,
                data_base64: STANDARD.encode(b"data"),
            },
        ))
        .await
        .unwrap();
    let finished = client
        .command(Command::Conversation(
            ConversationCommand::FinishAttachment { upload_id },
        ))
        .await
        .unwrap();
    let workbench_protocol::conversation::ConversationResult::AttachmentFinished(reference) =
        result(finished)
    else {
        panic!("the upload finishes");
    };
    reference.id
}

// Given: a follow-up queued behind a turn held at the provider
// When: the window is rebuilt, the follow-up is read back, taken back and sent again edited
//       under a new id
// Then: the withdrawn text never reaches the model, and the next request extends the
//       first one with only the edited text (an independent prefix check on the bodies the
//       mock received, whose cache is derived from them)
#[tokio::test(flavor = "multi_thread")]
async fn an_edited_queued_input_adds_only_its_new_text_to_the_model_prefix() {
    let (gate, arrived) = mock_openai::ResponseGate::new();
    let release = ReleaseOnDrop(gate.clone());
    let mut core = Core::start(
        vec![
            ScriptedResponse::text_stream("first", "mock-model", ["hello"]).with_gate(gate),
            ScriptedResponse::text_stream("second", "mock-model", ["edited answer"]),
        ],
        &[],
    )
    .await;
    let (actions, _shared, first_window) = window(&core).await;
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
    tokio::task::spawn_blocking(move || arrived.recv().unwrap())
        .await
        .unwrap();
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::SendChat(chat(
                "queued",
                "draft follow-up",
            ))),
            draft: None,
        })
        .await
        .unwrap();
    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, first_window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");

    // A window built afterwards knows nothing of the input but what the core tells it.
    let (actions, shared, window) = window(&core).await;
    actions
        .send(Action::History(
            ThreadId::new("thread").unwrap(),
            None,
            false,
        ))
        .await
        .unwrap();
    use workbench_protocol::views::InputReceiptState::{Accepted, Withdrawn};
    until(&shared, "the queued follow-up with its preview", |s| {
        receipt_in(s, "queued", Accepted).is_some_and(|r| r.preview.is_some())
    })
    .await;
    let queued = receipt_in(&shared.lock().unwrap(), "queued", Accepted).unwrap();
    assert_eq!(queued.preview.as_ref().unwrap().text, "draft follow-up");

    let client = protocol_client(&core, "e7-edit").await;
    let draft = read_draft(&client, "queued").await;
    assert_eq!(
        (
            draft.text.as_str(),
            draft.image_count,
            draft.attachment_count
        ),
        ("draft follow-up", 0, 0)
    );
    client
        .command(Command::Conversation(
            ConversationCommand::WithdrawFollowUp(workbench_protocol::commands::WithdrawFollowUp {
                thread: ThreadId::new("thread").unwrap(),
                run: queued.run.clone().unwrap(),
                submission_id: SubmissionId::new("queued").unwrap(),
            }),
        ))
        .await
        .unwrap();
    until(&shared, "the follow-up taken back", |s| {
        receipt_in(s, "queued", Withdrawn).is_some_and(|r| r.preview.is_none())
    })
    .await;

    release.0.release();
    turn_completed(&mut core.turns).await;
    // The edited text is a new input under a new id.
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::SendChat(chat(
                "queued-edited",
                "edited follow-up",
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
    assert_eq!(requests.len(), 2, "the taken-back input made no turn");
    assert_append_only(CacheProtocol::OpenAi, &requests[0].body, &requests[1].body).unwrap();
    let texts = |request: &mock_openai::RecordedRequest| user_texts(&request.body).join("\n");
    assert!(!texts(&requests[0]).contains("follow-up"));
    assert!(
        !texts(&requests[1]).contains("draft follow-up"),
        "the taken-back text was never sent"
    );
    assert!(texts(&requests[1]).contains("edited follow-up"));

    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");
    core.finish();
}

// Given: a follow-up with an image and a file, queued behind a held turn, which the core then
//        marks `Unknown` as it does for a run that no live incarnation can read
// When: a new window reads the receipts and asks for the words
// Then: the receipt shows its preview and counts, and the words come back whole with the
//       number of images to attach again; no attachment is returned
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_input_is_restorable_by_a_new_window_with_its_attachment_counts() {
    let (gate, arrived) = mock_openai::ResponseGate::new();
    let release = ReleaseOnDrop(gate.clone());
    let core = Core::start(
        vec![ScriptedResponse::text_stream("first", "mock-model", ["hello"]).with_gate(gate)],
        &[],
    )
    .await;
    let (actions, _shared, first_window) = window(&core).await;
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
    tokio::task::spawn_blocking(move || arrived.recv().unwrap())
        .await
        .unwrap();
    let client = protocol_client(&core, "e7-restore").await;
    let image = upload(&client, "shot.png", "image/png").await;
    let notes = upload(&client, "notes.txt", "text/plain").await;
    let mut follow_up = chat("queued", "look at the screenshot");
    follow_up.attachments = vec![image, notes];
    let accepted = client
        .command(Command::Conversation(ConversationCommand::SendChat(
            follow_up,
        )))
        .await
        .unwrap();
    assert_eq!(accepted.disposition, CommandDisposition::Accepted);
    let run = accepted.run_id.unwrap();
    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, first_window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");
    core._bridge
        .handle()
        .mark_host_inputs_unknown(run.as_str(), "the core restarted")
        .unwrap();

    let (actions, shared, window) = window(&core).await;
    actions
        .send(Action::History(
            ThreadId::new("thread").unwrap(),
            None,
            false,
        ))
        .await
        .unwrap();
    use workbench_protocol::views::InputReceiptState::Unknown;
    until(&shared, "the unknown input with its preview", |s| {
        receipt_in(s, "queued", Unknown).is_some_and(|r| r.preview.is_some())
    })
    .await;
    let preview = receipt_in(&shared.lock().unwrap(), "queued", Unknown)
        .unwrap()
        .preview
        .unwrap();
    assert_eq!(
        (
            preview.text.as_str(),
            preview.image_count,
            preview.attachment_count
        ),
        ("look at the screenshot", 1, 2)
    );
    let draft = read_draft(&protocol_client(&core, "e7-restore-2").await, "queued").await;
    assert_eq!(draft.text, "look at the screenshot");
    assert_eq!(draft.state, Unknown);
    assert!(!draft.text.contains('/'), "no attachment path in the words");

    drop(release);
    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");
    core.finish();
}

// Given: a follow-up with an image, queued behind a held turn, and a core restarted over the
//        same storage that marks the run's inputs `Unknown` as the owner registry does
// When: a new window reads the receipts and asks for the words
// Then: the preview, the counts and the whole words come back from the new core alone
#[tokio::test(flavor = "multi_thread")]
async fn a_planned_core_restart_keeps_a_queued_input_restorable() {
    let (gate, arrived) = mock_openai::ResponseGate::new();
    let release = ReleaseOnDrop(gate.clone());
    let core = Core::start(
        vec![ScriptedResponse::text_stream("first", "mock-model", ["hello"]).with_gate(gate)],
        &[],
    )
    .await;
    let (actions, _shared, first_window) = window(&core).await;
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
    tokio::task::spawn_blocking(move || arrived.recv().unwrap())
        .await
        .unwrap();
    let client = protocol_client(&core, "e7-restart").await;
    let image = upload(&client, "shot.png", "image/png").await;
    let mut follow_up = chat("queued", "look at the screenshot again");
    follow_up.attachments = vec![image];
    let accepted = client
        .command(Command::Conversation(ConversationCommand::SendChat(
            follow_up,
        )))
        .await
        .unwrap();
    assert_eq!(accepted.disposition, CommandDisposition::Accepted);
    let run = accepted.run_id.unwrap();
    drop(client);
    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, first_window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");

    let core = core.restart().await;
    core._bridge
        .handle()
        .mark_host_inputs_unknown(run.as_str(), "the core restarted")
        .unwrap();

    let (actions, shared, window) = window(&core).await;
    actions
        .send(Action::History(
            ThreadId::new("thread").unwrap(),
            None,
            false,
        ))
        .await
        .unwrap();
    use workbench_protocol::views::InputReceiptState::Unknown;
    until(&shared, "the unknown input with its preview", |s| {
        receipt_in(s, "queued", Unknown).is_some_and(|r| r.preview.is_some())
    })
    .await;
    let preview = receipt_in(&shared.lock().unwrap(), "queued", Unknown)
        .unwrap()
        .preview
        .unwrap();
    assert_eq!(
        (
            preview.text.as_str(),
            preview.image_count,
            preview.attachment_count
        ),
        ("look at the screenshot again", 1, 1)
    );
    let draft = read_draft(&protocol_client(&core, "e7-restart-2").await, "queued").await;
    assert_eq!(draft.text, "look at the screenshot again");
    assert_eq!((draft.state, draft.image_count), (Unknown, 1));

    drop(release);
    actions.send(Action::Shutdown).await.unwrap();
    tokio::time::timeout(WAIT, window)
        .await
        .expect("the actor ends when the window is closed")
        .expect("the actor does not panic");
    core.finish();
}
