// allow: SIZE_OK — Shared real-runtime contract fixture; task scope forbids extracting another file.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{
    AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent, OrchestratorEvent,
};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use gui::model::transcript::{ToolStatus, TranscriptEntry};
use gui::runtime_sink::RuntimeCommandSink;
use providers::Message;
use runtime::{AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor, OrchestrationSettings, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[path = "support/goal_capture_model.rs"]
mod goal_capture_model;
use goal_capture_model::CaptureModel;

struct ToolThenAnswer {
    capture: CaptureModel,
    tool_first: bool,
    seed_path: Option<std::path::PathBuf>,
}

#[async_trait::async_trait]
impl runtime::AgentModel for ToolThenAnswer {
    fn selected_model(&self, role: runtime::Role, preference: Option<&str>) -> String {
        self.capture.selected_model(role, preference)
    }

    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: runtime::Role,
        messages: &[Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        if self.tool_first
            && !messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, providers::ContentBlock::ToolResult { .. }))
        {
            self.capture
                .messages
                .lock()
                .unwrap()
                .push(messages.to_vec());
            return Ok(providers::ChatResponse {
                message: Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::ContentBlock::ToolUse {
                        id: "inspect-runs".into(),
                        name: "read".into(),
                        input: serde_json::json!({
                            "path": self
                                .seed_path
                                .as_ref()
                                .expect("tool_first requires seed_path")
                                .display()
                                .to_string(),
                        }),
                    }],
                },
                usage: providers::Usage::default(),
                finish_reason: providers::FinishReason::ToolUse,
            });
        }
        self.capture
            .complete(invocation, role, messages, tools)
            .await
    }
}

#[path = "support/goal_restore_contract.rs"]
mod goal_restore_contract;

struct Fixture {
    state: WorkbenchState<DemoSource>,
    sink: RuntimeCommandSink,
    runtime: AgentRuntime,
    rt: tokio::runtime::Runtime,
    events: EventReceiver,
    messages: Arc<Mutex<Vec<Vec<Message>>>>,
    created: usize,
    _storage: storage::Storage,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_success(false)
    }

    fn with_success(succeeds: bool) -> Self {
        Self::with_gate(succeeds, None)
    }

    fn with_gate(succeeds: bool, gate: Option<Arc<tokio::sync::Notify>>) -> Self {
        Self::with_script(succeeds, gate, false)
    }

    fn with_script(
        succeeds: bool,
        gate: Option<Arc<tokio::sync::Notify>>,
        tool_first: bool,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("test.sqlite3"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let bus = Arc::new(EventBus::new(512));
        let events = bus.subscribe();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let seed_path = tool_first.then(|| {
            let path = directory.path().join("seed.txt");
            std::fs::write(&path, "seed-read-marker").unwrap();
            path
        });
        let executor: Arc<tools::ToolExecutor> = if tool_first {
            Arc::new(tools::ToolExecutor::with_standard_tools(
                bus.clone(),
                Arc::new(sandbox::DirectSandbox::new_unchecked()),
            ))
        } else {
            Arc::new(tools::ToolExecutor::new(bus.clone()))
        };
        let runtime = AgentRuntime::new(
            bus.clone(),
            executor,
            Arc::new(ToolThenAnswer {
                capture: CaptureModel {
                    messages: messages.clone(),
                    succeeds,
                    gate,
                },
                tool_first,
                seed_path,
            }),
        )
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let supervisor = rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                bus,
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings {
                    max_continuations: 0,
                    ..OrchestrationSettings::default()
                },
            )
        });
        let sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor);
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("test");
        let thread = ThreadId::new("thread-1");
        sidebar
            .add_project(project.clone(), "test", directory.path())
            .unwrap();
        sidebar.select_project(&project).unwrap();
        sidebar
            .create_thread(thread.clone(), project, "test")
            .unwrap();
        sidebar.switch_thread(&thread).unwrap();
        let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
            .with_provider_status(ProviderStatus::Configured);
        Self {
            state,
            sink,
            runtime,
            rt,
            events,
            messages,
            created: 0,
            _storage: storage,
            _directory: directory,
        }
    }

    fn submit(&mut self, input: &str) -> Vec<LoopEvent> {
        self.state.composer_mut().input = input.into();
        self.state.submit_composer();
        let events = self
            .sink
            .submit(self.state.issued().last().unwrap().clone());
        for event in &events {
            self.state.apply_loop_event(event.clone());
        }
        events
    }

    fn terminal(&mut self) -> RunId {
        self.wait_phase(AgentRunPhase::Error)
    }

    fn wait_phase(&mut self, phase: AgentRunPhase) -> RunId {
        let id = self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = self.events.recv().await.unwrap();
                    self.state.apply_events([event.clone()]);
                    match event.kind {
                        EventKind::Orchestrator(OrchestratorEvent::GoalCreated { .. }) => {
                            self.created += 1
                        }
                        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                            run_id,
                            to,
                            ..
                        }) if to == phase => break run_id,
                        _ => {}
                    }
                }
            })
            .await
            .unwrap()
        });
        self.runtime
            .list_agents()
            .into_iter()
            .find(|run| run.run_id.to_string() == id)
            .unwrap()
            .run_id
    }
}

#[test]
fn follow_up_to_running_goal_is_consumed_before_root_finishes() {
    // Given: the goal's first completion is in flight.
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut fixture = Fixture::with_gate(true, Some(gate.clone()));
    fixture.submit("/goal fixture feature");
    let root = fixture.wait_phase(AgentRunPhase::Running);
    // When: ordinary chat is sent before the noninteractive root stops.
    let events = fixture.submit("follow-up while running");
    gate.notify_one();
    fixture.wait_phase(AgentRunPhase::Done);
    // Then: the goal root consumed the follow-up instead of dropping its inbox.
    assert!(
        matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string())
    );
    assert_eq!(fixture.messages.lock().unwrap().len(), 2);
}

#[test]
fn follow_up_to_done_goal_restores_context_and_live_follow_up_reuses_root() {
    // Given: a completed goal root with assistant history.
    let mut fixture = Fixture::with_success(true);
    fixture.submit("/goal fixture feature");
    let root = fixture.wait_phase(AgentRunPhase::Done);
    // When: two plain follow-ups resume the terminal root then address the live root.
    let first = fixture.submit("continue");
    assert_eq!(fixture.wait_phase(AgentRunPhase::Waiting), root);
    let second = fixture.submit("continue again");
    assert_eq!(fixture.wait_phase(AgentRunPhase::Waiting), root);
    // Then: both are routed to the original root and all user turns remain present.
    for events in [first, second] {
        assert!(
            matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string())
        );
    }
    assert_eq!(fixture.created, 1);
    let requests = fixture.messages.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let user_turns = requests[2]
        .iter()
        .filter(|message| message.role == providers::Role::User)
        .count();
    assert_eq!(user_turns, 3);
}

#[test]
fn follow_up_to_errored_goal_thread_resumes_goal_context() {
    // Given: an explicit goal root has failed after consuming its initial context.
    let mut fixture = Fixture::new();
    fixture.submit("/goal implement fixture feature");
    let root = fixture.terminal();
    let original = fixture.messages.lock().unwrap()[0].clone();
    // When: the user sends ordinary continuation text through the GUI composer.
    let events = fixture.submit("continue with the earlier constraints");
    fixture.terminal();
    // Then: the provider receives the goal history, not a detached fresh chat.
    let requests = fixture.messages.lock().unwrap();
    assert!(
        requests[1].starts_with(&original),
        "goal root context must be restored: {requests:#?}"
    );
    assert!(requests[1].len() > original.len());
    assert_eq!(fixture.created, 1);
    assert!(
        matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string())
    );
}

#[test]
fn plain_chat_never_increments_accepted_goals() {
    // Given: a fresh GUI composer and real runtime sink.
    let mut fixture = Fixture::new();
    // When: ordinary chat is submitted twice.
    for text in ["implement this feature", "continue"] {
        assert!(matches!(
            fixture.submit(text).as_slice(),
            [LoopEvent::ChatAccepted { .. }]
        ));
        fixture.terminal();
    }
    // Then: no supervisor goal exists and the first explicit goal remains goal-1.
    assert_eq!(fixture.created, 0);
    assert!(
        fixture
            .state
            .issued()
            .iter()
            .all(|command| matches!(command, WorkbenchCommand::SendChat(_)))
    );
    assert!(
        matches!(fixture.submit("/goal explicit").as_slice(), [LoopEvent::GoalAccepted { goal_id, .. }] if goal_id == "goal-1")
    );
    fixture.terminal();
    assert_eq!(fixture.created, 1);
}

#[test]
fn orchestrator_role_chat_full_flow_tool_and_answer() {
    // Given: the real runtime will execute the read tool before the scripted answer.
    let mut fixture = Fixture::with_script(true, None, true);
    fixture.state.composer_mut().toggle_role();
    // When: ordinary chat is submitted through the orchestrator composer.
    let events = fixture.submit("inspect the active runs");
    let run = fixture.wait_phase(AgentRunPhase::Waiting);
    // Then: tool execution and the assistant answer reach this chat, never a goal.
    assert!(
        matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == run.to_string())
    );
    let inspection = fixture.runtime.inspect_agent(run).unwrap();
    assert_eq!(inspection.role_name, runtime::Role::Orchestrator.name());
    assert_eq!(fixture.runtime.list_agents().len(), 1);
    assert_eq!(fixture.created, 0);
    assert_eq!(fixture.state.goal_form().last_accepted, None);
    let entries = fixture.state.transcripts().thread().entries();
    assert!(entries.iter().any(|entry| matches!(entry,
        TranscriptEntry::Message { text, run_id: Some(id) }
            if text == "fixture reply" && id == &run.to_string())));
    assert!(entries.iter().any(|entry| matches!(entry,
        TranscriptEntry::Tool { call_id, status: ToolStatus::Succeeded, is_error: false, .. }
            if call_id == "inspect-runs")));
    let requests = fixture.messages.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].iter().flat_map(|message| &message.content).any(|block|
        matches!(block, providers::ContentBlock::ToolResult { tool_call_id, is_error: false, content, .. }
            if tool_call_id == "inspect-runs" && content.iter().any(|part|
                matches!(part, providers::ToolResultContent::Text { text } if text.contains("seed-read-marker"))))));
    drop(requests);
    assert!(matches!(fixture.submit("/goal explicit").as_slice(),
        [LoopEvent::GoalAccepted { goal_id, .. }] if goal_id == "goal-1"));
}

#[test]
fn orchestrator_role_chat_after_error_continues_same_run() {
    // Given: an orchestrator chat failed after the provider consumed its context.
    let mut fixture = Fixture::new();
    fixture.state.composer_mut().toggle_role();
    fixture.submit("hello");
    let original_run = fixture.terminal();
    let original = fixture.messages.lock().unwrap()[0].clone();
    // When: a continuation is submitted from the same orchestrator composer.
    let events = fixture.submit("続けて");
    let continued = fixture.terminal();
    // Then: the same run keeps its role and context without creating a goal.
    assert_eq!(
        continued, original_run,
        "chat continuation must preserve run ID"
    );
    assert!(
        matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == original_run.to_string())
    );
    assert_eq!(
        fixture
            .runtime
            .list_agents()
            .iter()
            .find(|run| run.run_id == original_run)
            .unwrap()
            .role_name,
        runtime::Role::Orchestrator.name()
    );
    assert_eq!(fixture.created, 0);
    assert_eq!(fixture.state.goal_form().last_accepted, None);
    let entries = fixture.state.transcripts().thread().entries();
    for expected in ["hello", "続けて"] {
        assert!(entries.iter().any(|entry| matches!(entry,
            TranscriptEntry::UserMessage { text } if text == expected)));
    }
    let requests = fixture.messages.lock().unwrap();
    assert!(requests[1].starts_with(&original));
    assert_eq!(
        requests[1].last().unwrap().content,
        vec![providers::ContentBlock::Text {
            text: "続けて".into()
        }]
    );
    drop(requests);
    fixture.state.composer_mut().toggle_role();
    fixture.submit("keep the same role");
    assert_eq!(fixture.terminal(), original_run);
    assert_eq!(fixture.created, 0);
    assert!(
        matches!(fixture.submit("/goal explicit").as_slice(), [LoopEvent::GoalAccepted { goal_id, .. }] if goal_id == "goal-1")
    );
    fixture.terminal();
}

#[test]
fn explicit_goal_submission_increments_goal_counter() {
    // Given: a fresh GUI composer and real runtime sink.
    let mut fixture = Fixture::new();
    // When: two explicit /goal commands are submitted.
    for index in 1..=2 {
        let events = fixture.submit("/goal explicit");
        fixture.terminal();
        // Then: explicit submissions retain sequential goal-N numbering.
        assert!(
            matches!(events.as_slice(), [LoopEvent::GoalAccepted { goal_id, .. }] if *goal_id == format!("goal-{index}"))
        );
    }
    assert_eq!(fixture.created, 2);
}
