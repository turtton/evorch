use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent, MessageEvent};
use gui::model::commands::{ChatSubmission, CommandSink, LoopEvent, WorkbenchCommand};
use gui::runtime_sink::RuntimeCommandSink;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RunId, RuntimeError,
};
use tools::ToolExecutor;

#[derive(Debug)]
struct ObservedInvocation {
    role: Role,
    category: Option<String>,
    tools: Vec<String>,
}

struct ScriptedModel {
    invocations: Arc<Mutex<Vec<ObservedInvocation>>>,
    messages: Arc<Mutex<Vec<Vec<Message>>>>,
    responses: Mutex<VecDeque<ChatResponse>>,
    preferences: Arc<Mutex<Vec<Option<runtime::ModelPreference>>>>,
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.invocations.lock().unwrap().push(ObservedInvocation {
            role,
            category: invocation.category.clone(),
            tools: tools.iter().map(|tool| tool.name.clone()).collect(),
        });
        self.messages
            .lock()
            .expect("messages")
            .push(messages.to_vec());
        self.preferences
            .lock()
            .unwrap()
            .push(invocation.model_preference.clone());
        self.responses
            .lock()
            .expect("script lock")
            .pop_front()
            .ok_or_else(|| RuntimeError::Model {
                reason: "chat script exhausted".into(),
            })
    }

    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "test-chat".into()
    }
}

struct Fixture {
    invocations: Arc<Mutex<Vec<ObservedInvocation>>>,
    model: Arc<ScriptedModel>,
    _storage: storage::Storage,
    _directory: tempfile::TempDir,
    messages: Arc<Mutex<Vec<Vec<Message>>>>,
    rt: tokio::runtime::Runtime,
    sink: RuntimeCommandSink,
    runtime: AgentRuntime,
    events: EventReceiver,
    preferences: Arc<Mutex<Vec<Option<runtime::ModelPreference>>>>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let storage_config = storage::StorageConfig {
            db_path: directory.path().join("chat.sqlite3"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(storage_config.clone()).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("multi-thread test runtime");
        let bus = Arc::new(EventBus::new(64));
        let events = bus.subscribe();
        let responses = (1..=3)
            .map(|index| ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: format!("reply-{index}"),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
            .collect();
        let preferences = Arc::new(Mutex::new(Vec::new()));
        let invocations = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(Vec::new()));
        let model = Arc::new(ScriptedModel {
            invocations: invocations.clone(),
            messages: messages.clone(),
            responses: Mutex::new(responses),
            preferences: preferences.clone(),
        });
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(
                ToolExecutor::with_standard_tools(
                    bus.clone(),
                    Arc::new(sandbox::DirectSandbox::new_unchecked()),
                )
                .with_web_tools()
                .unwrap(),
            ),
            model.clone(),
        );
        let runtime = runtime
            .with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap());
        let supervisor = rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                bus,
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings::default(),
            )
        });
        let sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor);
        Self {
            invocations,
            model,
            _storage: storage,
            _directory: directory,
            messages,
            rt,
            sink,
            runtime,
            events,
            preferences,
        }
    }

    fn restart_runtime(&mut self) {
        let storage_config = storage::StorageConfig {
            db_path: self._directory.path().join("chat.sqlite3"),
            ..storage::StorageConfig::default()
        };
        let bus = Arc::new(EventBus::new(64));
        self.events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(
                ToolExecutor::with_standard_tools(
                    bus.clone(),
                    Arc::new(sandbox::DirectSandbox::new_unchecked()),
                )
                .with_web_tools()
                .unwrap(),
            ),
            self.model.clone(),
        )
        .with_run_store(runtime::RunStore::open(&storage_config, self._storage.handle()).unwrap());
        let supervisor = self.rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                bus,
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings::default(),
            )
        });
        self.sink = RuntimeCommandSink::new(runtime.clone(), self.rt.handle().clone(), supervisor);
        self.runtime = runtime;
    }

    fn send(&mut self, thread: &str, text: &str) -> String {
        self.send_preference(thread, text, None)
    }

    fn send_preference(
        &mut self,
        thread: &str,
        text: &str,
        model_preference: Option<runtime::ModelPreference>,
    ) -> String {
        let events = self.sink.submit(WorkbenchCommand::SendChat(ChatSubmission {
            composer_role: gui::model::composer::ComposerRole::Worker,
            images: Vec::new(),
            thread_id: thread.into(),
            text: text.into(),
            model_preference,
        }));
        match events.as_slice() {
            [LoopEvent::ChatAccepted { thread_id, run_id }] => {
                assert_eq!(thread_id, thread);
                run_id.clone()
            }
            other => panic!("expected ChatAccepted, got {other:?}"),
        }
    }

    fn wait_for_reply(&mut self, run_id: &str, reply: &str) {
        self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut observed_reply = false;
                loop {
                    let event = self.events.recv().await.expect("chat event");
                    match event.kind {
                        EventKind::Message(MessageEvent::MessageDelta {
                            delta,
                            run_id: Some(id),
                        }) if id == run_id => {
                            assert_eq!(delta, reply);
                            observed_reply = true;
                        }
                        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                            run_id: id,
                            to: AgentRunPhase::Waiting,
                            ..
                        }) if id == run_id => {
                            assert!(observed_reply, "Waiting must follow the reply");
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("reply and Waiting within 5s");
        });
        let agent = self
            .runtime
            .inspect_agent(self.run_id(run_id))
            .expect("chat run");
        assert_eq!(agent.phase, AgentRunPhase::Waiting);
    }

    fn run_id(&self, id: &str) -> RunId {
        self.runtime
            .list_agents()
            .iter()
            .find(|agent| agent.run_id.to_string() == id)
            .expect("accepted run exists")
            .run_id
    }
}

#[test]
fn composer_chat_grants_conversation_only_to_worker_on_start_and_restore() {
    use gui::model::composer::ComposerRole;
    for (composer_role, role, category) in [
        (ComposerRole::Worker, Role::Worker, Some("conversation")),
        (ComposerRole::Orchestrator, Role::Orchestrator, None),
    ] {
        let mut fixture = Fixture::new();
        let mut previous = None;
        for (text, reply) in [("turn-1", "reply-1"), ("turn-2", "reply-2")] {
            let events = fixture
                .sink
                .submit(WorkbenchCommand::SendChat(ChatSubmission {
                    composer_role,
                    images: Vec::new(),
                    thread_id: "conversation".into(),
                    text: text.into(),
                    model_preference: None,
                }));
            let [LoopEvent::ChatAccepted { run_id, .. }] = events.as_slice() else {
                panic!("expected accepted chat: {events:?}");
            };
            fixture.wait_for_reply(run_id, reply);
            if let Some(previous) = &previous {
                assert_eq!(run_id, previous, "terminal chat continues in place");
            }
            let id = fixture.run_id(run_id);
            let agent = fixture
                .runtime
                .list_agents()
                .into_iter()
                .find(|agent| agent.run_id == id)
                .unwrap();
            assert_eq!(agent.role_name, role.name());
            assert_eq!(agent.category.as_deref(), category);
            fixture.runtime.cancel(id).unwrap();
            fixture.rt.block_on(fixture.runtime.wait(id)).unwrap();
            previous = Some(run_id.clone());
        }
        // Observe the actual policy at the provider boundary: Worker search requires
        // both conversation=true and the category in each freshly constructed RunConfig.
        let invocations = fixture.invocations.lock().unwrap();
        assert_eq!(invocations.len(), 2);
        for invocation in invocations.iter() {
            assert_eq!(invocation.role, role);
            assert_eq!(invocation.category.as_deref(), category);
            assert_eq!(
                invocation.tools.iter().any(|tool| tool == "web_search"),
                role == Role::Worker
            );
            assert!(invocation.tools.iter().any(|tool| tool == "web_fetch"));
            assert_eq!(
                invocation.tools.iter().any(|tool| tool == "edit"),
                role == Role::Worker
            );
        }
    }
}

#[test]
fn terminal_chat_continuation_uses_saved_role_after_composer_changes() {
    use gui::model::commands::ChatContinuation;
    use gui::model::composer::ComposerRole;

    let config = config::Config::default();
    let catalog = Arc::new(
        runtime::build_catalog(&runtime::CatalogBuildInput {
            config: &config,
            user_presets_dir: None,
            available_agents: &[],
            available_skills: &[],
        })
        .unwrap(),
    );
    for (saved_composer, next_composer, role, category) in [
        (
            ComposerRole::Worker,
            ComposerRole::Orchestrator,
            Role::Worker,
            Some("conversation"),
        ),
        (
            ComposerRole::Orchestrator,
            ComposerRole::Worker,
            Role::Orchestrator,
            None,
        ),
    ] {
        // A normal followup reuses the sink's run binding; /continue also resolves
        // the saved root when a fresh runtime has no registered runs or binding.
        for (resume_only, restart) in [(false, false), (true, false), (true, true)] {
            let mut fixture = Fixture::new();
            fixture.runtime = fixture.runtime.clone().with_system_prompts(catalog.clone());
            let events = fixture
                .sink
                .submit(WorkbenchCommand::SendChat(ChatSubmission {
                    composer_role: saved_composer,
                    images: Vec::new(),
                    thread_id: "conversation".into(),
                    text: "turn-1".into(),
                    model_preference: None,
                }));
            let [LoopEvent::ChatAccepted { run_id, .. }] = events.as_slice() else {
                panic!("expected accepted chat: {events:?}");
            };
            fixture.wait_for_reply(run_id, "reply-1");
            let id = fixture.run_id(run_id);
            let events = fixture.sink.submit(WorkbenchCommand::StopChat {
                thread_id: "conversation".into(),
            });
            assert!(matches!(events.as_slice(), [LoopEvent::ChatStopped { .. }]));
            assert_eq!(
                fixture.rt.block_on(fixture.runtime.wait(id)).unwrap(),
                AgentRunPhase::Stopped
            );
            if restart {
                fixture.restart_runtime();
                fixture.runtime = fixture.runtime.clone().with_system_prompts(catalog.clone());
                assert!(fixture.runtime.list_agents().is_empty());
            }
            assert_eq!(
                fixture
                    .runtime
                    .restore_diagnostics(id)
                    .unwrap()
                    .unwrap()
                    .role_name,
                role.name(),
                "saved role is available even without a registered run"
            );
            let command = if resume_only {
                WorkbenchCommand::ContinueChat(ChatContinuation {
                    thread_id: "conversation".into(),
                    composer_role: next_composer,
                    model_preference: None,
                })
            } else {
                WorkbenchCommand::SendChat(ChatSubmission {
                    composer_role: next_composer,
                    images: Vec::new(),
                    thread_id: "conversation".into(),
                    text: "turn-2".into(),
                    model_preference: None,
                })
            };
            let events = fixture.sink.submit(command);
            let [
                LoopEvent::ChatAccepted {
                    run_id: continued, ..
                },
            ] = events.as_slice()
            else {
                panic!("expected accepted continuation: {events:?}");
            };
            assert_eq!(continued, run_id, "saved root continues in place");
            fixture.wait_for_reply(continued, "reply-2");
            let agents = fixture.runtime.list_agents();
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].role_name, role.name());
            assert_eq!(agents[0].category.as_deref(), category);
            fixture.runtime.cancel(id).unwrap();
            fixture.rt.block_on(fixture.runtime.wait(id)).unwrap();

            let invocations = fixture.invocations.lock().unwrap();
            assert_eq!(invocations.len(), 2);
            for invocation in invocations.iter() {
                assert_eq!(invocation.role, role);
                assert_eq!(invocation.category.as_deref(), category);
                // Search requires conversation=true as well as the category and root.
                assert_eq!(
                    invocation.tools.iter().any(|tool| tool == "web_search"),
                    role == Role::Worker
                );
                assert!(invocation.tools.iter().any(|tool| tool == "web_fetch"));
                assert_eq!(
                    invocation.tools.iter().any(|tool| tool == "edit"),
                    role == Role::Worker
                );
            }
            let messages = fixture.messages.lock().unwrap();
            assert_eq!(messages[0][0].role, MessageRole::System);
            assert!(messages[1].starts_with(&messages[0]));
            let continued_prompt = if resume_only {
                AgentRuntime::CHAT_CONTINUE_PROMPT
            } else {
                "turn-2"
            };
            assert_eq!(
                messages[1].last().unwrap().content,
                vec![ContentBlock::Text {
                    text: continued_prompt.into()
                }]
            );
        }
    }
}

#[test]
fn sink_followup_restores_terminal_chat_context() {
    // Given: the sink still remembers a chat run that has terminated.
    let mut fixture = Fixture::new();
    let first = fixture.send("thread", "turn-1");
    fixture.wait_for_reply(&first, "reply-1");
    let first_id = fixture.run_id(&first);
    fixture.runtime.cancel(first_id).unwrap();
    fixture.rt.block_on(fixture.runtime.wait(first_id)).unwrap();
    // When: the terminated run is continued in place with restored history.
    let second = fixture.send("thread", "turn-2");
    fixture.wait_for_reply(&second, "reply-2");
    // Then: the same run continues and its provider request keeps the prior turns.
    assert_eq!(first, second);
    let messages = fixture.messages.lock().unwrap();
    let conversational: Vec<_> = messages[1]
        .iter()
        .filter(|message| message.role != MessageRole::System)
        .collect();
    assert_eq!(conversational.len(), 3);
    for (message, role, text) in [
        (conversational[0], MessageRole::User, "turn-1"),
        (conversational[1], MessageRole::Assistant, "reply-1"),
        (conversational[2], MessageRole::User, "turn-2"),
    ] {
        assert_eq!(message.role, role);
        assert_eq!(
            message.content,
            vec![ContentBlock::Text { text: text.into() }]
        );
    }
}

#[test]
fn gui_paste_send_reaches_model_on_first_and_followup_turns() {
    use egui_kittest::kittest::Queryable;
    let mut fixture = Fixture::new();
    let temp = tempfile::tempdir().expect("project");
    let mut sidebar = workspace_ui::SidebarState::default();
    let project = workspace_ui::ProjectId::new("test");
    let thread = workspace_ui::ThreadId::new("thread-1");
    sidebar
        .add_project(project.clone(), "test", temp.path())
        .expect("project");
    sidebar.select_project(&project).expect("select");
    sidebar
        .create_thread(thread.clone(), project, "chat")
        .expect("thread");
    sidebar.switch_thread(&thread).expect("switch");
    let mut state = gui::app::WorkbenchState::new(
        gui::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .expect("state")
    .with_sidebar(sidebar)
    .with_provider_status(gui::model::composer::ProviderStatus::Configured);
    state.composer_mut().image_input_supported = true;
    let mut harness = egui_kittest::Harness::builder().build_ui_state(
        |ui, state: &mut gui::app::WorkbenchState<gui::fixture::DemoSource>| {
            state.ui(ui, &mut eframe::Frame::_new_kittest());
        },
        state,
    );
    let mut ids = Vec::new();
    for reply in ["reply-1", "reply-2"] {
        harness.event(egui::Event::Paste("data:image/png;base64,aGVsbG8=".into()));
        harness.run_steps(4);
        harness.get_by_label("Send").click();
        harness.run_steps(4);
        let command = harness
            .state()
            .issued()
            .last()
            .expect("GUI command")
            .clone();
        let events = fixture.sink.submit(command);
        let [LoopEvent::ChatAccepted { run_id, .. }] = events.as_slice() else {
            panic!("accepted");
        };
        fixture.wait_for_reply(run_id, reply);
        ids.push(run_id.clone());
    }
    assert_eq!(ids[0], ids[1]);
    let requests = fixture.messages.lock().expect("messages");
    for request in requests.iter() {
        let user = request
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .expect("user");
        assert!(user.content.iter().any(
            |block| matches!(block, ContentBlock::Image { media_type, data }
            if media_type == "image/png" && data == "aGVsbG8=")
        ));
    }
    assert_eq!(requests.len(), 2);
}

#[test]
fn sink_sets_preference_on_existing_run_before_send() {
    // Given: the real runtime with a recording model, not a mocked send path.
    let mut fixture = Fixture::new();
    let first = Some(runtime::ModelPreference {
        profile: "local".into(),
        model: Some("a".into()),
    });
    let second = Some(runtime::ModelPreference {
        profile: "remote".into(),
        model: Some("b".into()),
    });
    let id = fixture.send_preference("thread-1", "first", first.clone());
    fixture.wait_for_reply(&id, "reply-1");
    // When: another turn changes the preference on the keep-alive run.
    let reused = fixture.send_preference("thread-1", "second", second.clone());
    fixture.wait_for_reply(&reused, "reply-2");
    // Then: the first RunConfig and the update both reach the completion boundary.
    assert_eq!(reused, id);
    assert_eq!(*fixture.preferences.lock().unwrap(), vec![first, second]);
}

#[test]
fn first_chat_spawns_keep_alive_worker_run() {
    // Given
    let mut fixture = Fixture::new();
    // When: submit outside an entered tokio runtime, as the GUI does.
    let id = fixture.send("thread-1", "hello");
    // Then
    fixture.wait_for_reply(&id, "reply-1");
    let agents = fixture.runtime.list_agents();
    let agent = agents
        .iter()
        .find(|agent| agent.run_id.to_string() == id)
        .expect("chat run");
    assert_eq!(agent.name, "chat:Worker:thread-1");
    assert_eq!(agent.role_name, "Worker");
}

#[test]
fn second_chat_reuses_same_run_and_run_survives_resume() {
    // Given
    let mut fixture = Fixture::new();
    let first = fixture.send("thread-1", "hello");
    fixture.wait_for_reply(&first, "reply-1");
    // When
    let second = fixture.send("thread-1", "again");
    // Then
    assert_eq!(second, first);
    fixture.wait_for_reply(&second, "reply-2");
}

#[test]
fn chat_to_terminated_run_continues_same_run() {
    // Given
    let mut fixture = Fixture::new();
    let first = fixture.send("thread-1", "hello");
    fixture.wait_for_reply(&first, "reply-1");
    let run_id = fixture.run_id(&first);
    fixture.runtime.cancel(run_id).expect("cancel chat");
    let phase = fixture.rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), fixture.runtime.wait(run_id))
            .await
            .expect("cancel within 5s")
            .expect("cancelled run")
    });
    assert_eq!(phase, AgentRunPhase::Error);
    // When
    let second = fixture.send("thread-1", "again");
    // Then
    assert_eq!(second, first);
    fixture.wait_for_reply(&second, "reply-2");
}

#[test]
fn chats_on_two_threads_use_distinct_runs() {
    // Given
    let mut fixture = Fixture::new();
    let first = fixture.send("t1", "hello");
    // When
    let second = fixture.send("t2", "hello");
    // Then
    assert_ne!(second, first);
    assert_ne!(fixture.run_id(&first), fixture.run_id(&second));
}

#[test]
fn stop_retains_chat_history_and_resumes_same_run_with_current_model_preference() {
    let mut fixture = Fixture::new();
    let root = fixture.send("thread-stop", "first");
    fixture.wait_for_reply(&root, "reply-1");
    let events = fixture.sink.submit(WorkbenchCommand::StopChat {
        thread_id: "thread-stop".into(),
    });
    assert!(matches!(
        events.as_slice(),
        [LoopEvent::ChatStopped {
            running_children: 0,
            ..
        }]
    ));
    let run = fixture.run_id(&root);
    let phase = fixture.rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), fixture.runtime.wait(run))
            .await
            .unwrap()
            .unwrap()
    });
    assert_eq!(phase, AgentRunPhase::Stopped);
    let preference = runtime::ModelPreference {
        profile: "current".into(),
        model: Some("new-model".into()),
    };
    assert_eq!(
        fixture.send_preference("thread-stop", "resume", Some(preference.clone())),
        root
    );
    fixture.wait_for_reply(&root, "reply-2");
    assert_eq!(
        fixture.preferences.lock().unwrap().last(),
        Some(&Some(preference))
    );
    let requests = fixture.messages.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "reply-1"))
    }));
}
