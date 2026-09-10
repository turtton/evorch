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

struct ScriptedModel {
    messages: Arc<Mutex<Vec<Vec<Message>>>>,
    responses: Mutex<VecDeque<ChatResponse>>,
    preferences: Arc<Mutex<Vec<Option<runtime::ModelPreference>>>>,
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
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

    fn selected_model(&self, _role: Role) -> String {
        "test-chat".into()
    }
}

struct Fixture {
    messages: Arc<Mutex<Vec<Vec<Message>>>>,
    rt: tokio::runtime::Runtime,
    sink: RuntimeCommandSink,
    runtime: AgentRuntime,
    events: EventReceiver,
    preferences: Arc<Mutex<Vec<Option<runtime::ModelPreference>>>>,
}

impl Fixture {
    fn new() -> Self {
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
        let messages = Arc::new(Mutex::new(Vec::new()));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            Arc::new(ScriptedModel {
                messages: messages.clone(),
                responses: Mutex::new(responses),
                preferences: preferences.clone(),
            }),
        );
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
            messages,
            rt,
            sink,
            runtime,
            events,
            preferences,
        }
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
    assert_eq!(agent.name, "chat:thread-1");
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
fn chat_to_terminated_run_respawns() {
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
    assert_ne!(second, first);
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
