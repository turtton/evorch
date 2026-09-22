use event_bus::{
    AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent, OrchestratorEvent,
};
use gui::{
    model::commands::{ChatSubmission, CommandSink, GoalSubmission, LoopEvent, WorkbenchCommand},
    runtime_sink::RuntimeCommandSink,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RunConfig, RunId, RunStore, RuntimeError,
};
use std::sync::{Arc, Mutex};

struct CaptureModel(Arc<Mutex<Vec<Vec<Message>>>>);
#[async_trait::async_trait]
impl AgentModel for CaptureModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.0.lock().unwrap().push(messages.to_vec());
        if messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "held child"))
        {
            std::future::pending::<()>().await;
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "answer".into(),
                }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    config: storage::StorageConfig,
    storage: storage::Storage,
    rt: tokio::runtime::Runtime,
    messages: Arc<Mutex<Vec<Vec<Message>>>>,
}
struct World {
    runtime: AgentRuntime,
    sink: RuntimeCommandSink,
    events: EventReceiver,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("gui.sqlite3"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
            messages: Arc::default(),
        }
    }
    fn world(&self) -> World {
        let bus = Arc::new(EventBus::new(256));
        let events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            Arc::new(CaptureModel(self.messages.clone())),
        )
        .with_run_store(RunStore::open(&self.config, self.storage.handle()).unwrap());
        let supervisor = self.rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                bus,
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings {
                    max_continuations: 0,
                    ..Default::default()
                },
            )
        });
        let sink = RuntimeCommandSink::new(runtime.clone(), self.rt.handle().clone(), supervisor)
            .with_memory_storage(self.config.clone())
            .with_team_writer(self.storage.handle());
        World {
            runtime,
            sink,
            events,
        }
    }
    fn root(&self, world: &mut World, team: bool) -> RunId {
        let result = world
            .sink
            .submit(WorkbenchCommand::SubmitGoal(GoalSubmission {
                delegation_value: team.then(|| "independent work".into()),
                project_id: "project".into(),
                thread_id: "thread".into(),
                goal: "direct: original goal".into(),
                references: Vec::new(),
                constraints: Vec::new(),
            }));
        assert!(matches!(
            result.as_slice(),
            [LoopEvent::GoalAccepted { .. }]
        ));
        self.rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let mut root = None;
                loop {
                    let event = world.events.recv().await.unwrap();
                    match event.kind {
                        EventKind::Orchestrator(OrchestratorEvent::GoalCreated {
                            root_run_id,
                            ..
                        }) => root = Some(root_run_id),
                        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                            run_id,
                            to: AgentRunPhase::Done,
                            ..
                        }) if root.as_ref() == Some(&run_id) => return parse(&run_id),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap()
        })
    }
    fn waiting(&self, world: &mut World, root: RunId) {
        self.rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if matches!(world.events.recv().await.unwrap().kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == root.to_string()) { break; }
                }
            }).await.unwrap();
        });
    }
    fn stop(&self, world: &World, root: RunId) {
        world.runtime.cancel(root).unwrap();
        self.rt.block_on(world.runtime.wait(root)).unwrap();
    }
}
fn parse(id: &str) -> RunId {
    RunId::new(id.strip_prefix("run-").unwrap().parse().unwrap())
}
fn send(world: &mut World, text: &str) -> Vec<LoopEvent> {
    world
        .sink
        .submit(WorkbenchCommand::SendChat(ChatSubmission {
            thread_id: "thread".into(),
            text: text.into(),
            composer_role: Default::default(),
            images: Vec::new(),
            model_preference: None,
        }))
}
fn accepted(events: &[LoopEvent], root: RunId) {
    assert!(
        matches!(events, [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string()),
        "{events:?}"
    );
}
fn rejected(events: &[LoopEvent], reason: &str) {
    assert!(
        matches!(events, [LoopEvent::ChatRejected { thread_id, reason: actual }] if thread_id == "thread" && actual.contains(reason)),
        "{events:?}"
    );
    let mut app = gui::app::WorkbenchState::new(
        gui::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .unwrap();
    app.apply_loop_event(events[0].clone());
    assert!(
        app.transcript()
            .entries()
            .iter()
            .any(|entry| matches!(entry,
        gui::model::transcript::TranscriptEntry::Notice { text }
        if text.starts_with("chat failed:") && text.contains(reason)))
    );
}

#[test]
fn fresh_and_restarted_team_goals_continue_with_current_project_authority() {
    let fixture = Fixture::new();
    let mut first = fixture.world();
    let root = fixture.root(&mut first, true);
    accepted(&send(&mut first, "fresh followup"), root);
    fixture.waiting(&mut first, root);
    fixture.stop(&first, root);
    let mut restarted = fixture.world();
    restarted
        .sink
        .bind_goal_context("thread", "project", &root.to_string());
    accepted(&send(&mut restarted, "restart followup"), root);
    fixture.waiting(&mut restarted, root);
    let requests = fixture.messages.lock().unwrap();
    let last = serde_json::to_string(requests.last().unwrap()).unwrap();
    assert!(last.contains("original goal"));
    assert!(last.contains("fresh followup"));
    assert!(last.contains("restart followup"));
    drop(requests);
    fixture.stop(&restarted, root);
}
#[test]
fn another_project_or_team_cannot_reuse_the_saved_team_authority() {
    let fixture = Fixture::new();
    let mut original = fixture.world();
    let root = fixture.root(&mut original, true);
    let mut restarted = fixture.world();
    restarted
        .sink
        .bind_goal_context("thread", "another-project", &root.to_string());
    rejected(
        &send(&mut restarted, "continue"),
        "current_team_authority_required",
    );
    assert_eq!(fixture.messages.lock().unwrap().len(), 1);
}
#[test]
fn single_goal_memory_and_finding_store_do_not_prevent_gui_followup() {
    let fixture = Fixture::new();
    let mut world = fixture.world();
    let root = fixture.root(&mut world, false);
    assert!(
        world
            .runtime
            .restore_diagnostics(root)
            .unwrap()
            .unwrap()
            .history_available_with_current_authority
    );
    accepted(&send(&mut world, "single followup"), root);
    fixture.waiting(&mut world, root);
    fixture.stop(&world, root);
    assert!(
        world
            .runtime
            .restore_diagnostics(root)
            .unwrap()
            .unwrap()
            .disk_restorable
    );
}
#[test]
fn team_busy_descendant_and_claim_failures_reach_chat_rejection() {
    let fixture = Fixture::new();
    let mut world = fixture.world();
    let root = fixture.root(&mut world, true);
    let child = fixture.rt.block_on(async {
        world
            .runtime
            .delegate_background_as_child(root, Role::Explorer, "held child", RunConfig::default())
            .unwrap()
    });
    rejected(&send(&mut world, "continue"), "descendant is still running");
    fixture.stop(&world, child);
    let board =
        runtime::team::TeamBoard::durable(fixture.storage.handle(), "project:thread".into());
    board
        .enqueue(runtime::team::TaskSpec {
            id: "uncertain".into(),
            paths: vec!["src".into()],
        })
        .unwrap();
    board
        .claim("uncertain", "stopped-worker", u64::MAX - 10_000)
        .unwrap();
    rejected(&send(&mut world, "continue"), "persisted task claims");
}
