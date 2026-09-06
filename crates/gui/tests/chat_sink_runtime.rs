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

struct ScriptedModel(Mutex<VecDeque<ChatResponse>>);

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.0
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
    rt: tokio::runtime::Runtime,
    sink: RuntimeCommandSink,
    runtime: AgentRuntime,
    events: EventReceiver,
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
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            Arc::new(ScriptedModel(Mutex::new(responses))),
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
            rt,
            sink,
            runtime,
            events,
        }
    }

    fn send(&mut self, thread: &str, text: &str) -> String {
        let events = self.sink.submit(WorkbenchCommand::SendChat(ChatSubmission {
            thread_id: thread.into(),
            text: text.into(),
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
