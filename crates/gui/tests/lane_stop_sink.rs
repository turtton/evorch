//! A lane root the runtime starts itself is stopped by the lane thread's Stop through the
//! real `RuntimeCommandSink`, and the lead that opened the lane keeps running.
use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use gui::{
    app::WorkbenchState, fixture::DemoSource, model::composer::ProviderStatus,
    runtime_sink::RuntimeCommandSink,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RunStore,
    RuntimeError,
};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

/// The lead answers and waits; a lane root (it sees the agent brief) never gets a reply.
#[derive(Default)]
struct LaneModel {
    lane_turn: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl AgentModel for LaneModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let lane = messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("Agent-authored lane brief"))
            })
        });
        if lane {
            self.lane_turn.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "ready".into(),
                }],
            },
            usage: Default::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "lane-stop".into()
    }
}

fn brief() -> event_bus::LaneBrief {
    event_bus::LaneBrief {
        objective: "an independent deliverable".into(),
        inputs: vec![],
        deliverables: vec!["artifact".into()],
        acceptance_criteria: vec!["review".into()],
        constraints: vec![],
        workspace: Default::default(),
        report_policy: event_bus::LanePolicy::Attached,
    }
}

/// Feeds bus events to the workbench until `done` holds for the workbench.
async fn pump_until(
    state: &mut WorkbenchState<DemoSource>,
    events: &mut EventReceiver,
    done: impl Fn(&WorkbenchState<DemoSource>, &EventKind) -> bool,
) {
    loop {
        let event = events.recv().await.unwrap();
        let kind = event.kind.clone();
        state.apply_events([event]);
        if done(state, &kind) {
            return;
        }
    }
}

fn phase_is(kind: &EventKind, run: RunId, wanted: AgentRunPhase) -> bool {
    matches!(kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
        if *run_id == run.to_string() && *to == wanted)
}

#[test]
fn stopping_a_lane_thread_stops_the_runtime_started_lane_root_and_leaves_the_lead_running() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("lanes.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let mut events = bus.subscribe();
    let model = Arc::new(LaneModel::default());
    let runtime = {
        let _guard = rt.enter();
        AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap())
    };
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("lead"), project, "Lead")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("lead")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(RuntimeCommandSink::new(
            runtime.clone(),
            rt.handle().clone(),
        )));

    rt.block_on(async {
        let lead = runtime
            .delegate_chat(
                "lead",
                Role::Orchestrator,
                "coordinate".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
        pump_until(&mut state, &mut events, |_, kind| {
            phase_is(kind, lead, AgentRunPhase::Waiting)
        })
        .await;

        // The runtime starts the lane root itself; this window never submitted to it.
        let lane = runtime.open_lane(lead, "stop-me", brief()).unwrap();
        let lane_thread = ThreadId::new(lane.thread_id.clone());
        let root = lane.root_run_id.to_string();
        model.lane_turn.notified().await;
        pump_until(&mut state, &mut events, |state, _| {
            state.sidebar().threads.iter().any(|thread| {
                thread.id == lane_thread && thread.root_run_id.as_deref() == Some(root.as_str())
            })
        })
        .await;

        state.switch_thread(lane_thread).unwrap();
        state.stop_chat();
        pump_until(&mut state, &mut events, |_, kind| {
            phase_is(kind, lane.root_run_id, AgentRunPhase::Stopped)
        })
        .await;
        assert_eq!(
            runtime.inspect_agent(lead).unwrap().phase,
            AgentRunPhase::Waiting,
            "the lead is not part of the lane's Stop"
        );

        runtime.cancel(lead).unwrap();
        runtime.wait(lead).await.unwrap();
    });
}
