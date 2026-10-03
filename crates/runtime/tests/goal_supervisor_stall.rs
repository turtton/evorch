mod support;

use std::sync::Arc;

use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, GoalState, LifecycleEvent,
    OrchestratorEvent, RunPurpose, StallSignal, ToolEvent,
};
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::OrchestrationSettings;
use runtime::orchestration::stall::{ProgressTrack, judge};
use runtime::orchestration::supervisor::{GoalSpec, GoalSupervisor};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tools::{ShellCommandContract, ToolExecutor};

use support::ScriptedModel;

struct Fixture {
    runtime: AgentRuntime,
    bus: Arc<EventBus>,
    handle: runtime::orchestration::supervisor::SupervisorHandle,
    events: EventReceiver,
    parent: runtime::RunId,
    child: runtime::RunId,
    goal_id: String,
}

impl Fixture {
    async fn new(max_nudges: u32) -> Self {
        let bus = Arc::new(EventBus::new(512));
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(
            Arc::clone(&bus),
            executor,
            Arc::new(ScriptedModel::gated([], Arc::new(Notify::new()))),
        );
        let settings = OrchestrationSettings {
            stall_after_secs: 0,
            stall_check_secs: 1,
            in_flight_tool_multiplier: 3,
            repeated_error_threshold: 3,
            max_nudges,
            ..OrchestrationSettings::default()
        };
        let handle = GoalSupervisor::spawn(
            runtime.clone(),
            Arc::clone(&bus),
            Arc::new(FixtureDeliveryAdapter::default()),
            settings,
        );
        let mut events = handle.subscribe();
        let parent =
            runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "WORK", RunConfig::default())
            .expect("child");
        let goal_id = handle.create_goal(
            GoalSpec {
                session_id: "session-stall".into(),
                project_id: "evorch".into(),
                thread_id: "thread-stall".into(),
                goal: "work".into(),
                references: vec![],
                constraints: vec![],
                repo: "turtton/evorch".into(),
                base_ref: "main".into(),
            },
            parent,
        );
        bus.emit(Event::new(OrchestratorEvent::RunAttached {
            goal_id: goal_id.clone(),
            run_id: child.to_string(),
            parent_run_id: Some(parent.to_string()),
            role: "worker".into(),
            purpose: RunPurpose::Implement,
        }));
        // Both runs must finish initialization before advancing the stall clock:
        // a late Running event would reset the progress timestamp and counter.
        while [parent, child].iter().any(|run| {
            runtime.inspect_agent(*run).expect("registered run").phase != AgentRunPhase::Running
        }) {
            events.recv().await.expect("run initialization event");
        }
        handle
            .synchronize()
            .await
            .expect("initial progress applied");
        Self {
            runtime,
            bus,
            handle,
            events,
            parent,
            child,
            goal_id,
        }
    }

    async fn tick(&self) {
        self.handle.synchronize().await.expect("progress applied");
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
    }

    async fn wait_for(&mut self, expected: impl Fn(&OrchestratorEvent) -> bool) {
        loop {
            if let EventKind::Orchestrator(event) =
                self.events.recv().await.expect("supervisor event").kind
                && expected(&event)
            {
                return;
            }
        }
    }

    async fn wait_for_nudge(&mut self, index: u32) {
        let child = self.child.to_string();
        self.wait_for(|event| {
            matches!(event,
                OrchestratorEvent::NudgeSent { run_id, nudge_index, .. }
                    if run_id == &child && *nudge_index == index
            )
        })
        .await;
    }

    fn progress(&self) {
        self.bus
            .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: self.child.to_string(),
                from: AgentRunPhase::Pending,
                to: AgentRunPhase::Running,
                reason: None,
            }));
    }
}

#[tokio::test(start_paused = true)]
async fn no_progress_after_stall_window_sends_steering_nudge_from_parent() {
    let mut fixture = Fixture::new(2).await;
    fixture.tick().await;
    fixture.wait_for_nudge(1).await;
    let message = fixture
        .runtime
        .take_inbox(fixture.child)
        .expect("mailbox")
        .pop()
        .expect("nudge");
    assert_eq!(message.sender_run_id, fixture.parent.to_string());
    assert_eq!(message.kind, event_bus::AgentMessageKind::Steering);
}

#[test]
fn in_flight_tool_gets_multiplied_window() {
    let now = tokio::time::Instant::now();
    let mut track = ProgressTrack::new(AgentRunPhase::Running);
    track.last_progress = now;
    track.tool_in_flight = Some(now);
    let settings = OrchestrationSettings {
        stall_after_secs: 10,
        in_flight_tool_multiplier: 3,
        ..OrchestrationSettings::default()
    };
    assert_eq!(
        judge(&track, now + std::time::Duration::from_secs(20), &settings),
        None
    );
    assert_eq!(
        judge(&track, now + std::time::Duration::from_secs(31), &settings),
        Some(StallSignal::NoProgress)
    );
}

#[tokio::test(start_paused = true)]
async fn progress_resets_nudge_counter() {
    let mut fixture = Fixture::new(2).await;
    fixture.tick().await;
    fixture.wait_for_nudge(1).await;
    fixture.progress();
    fixture.tick().await;
    fixture.wait_for_nudge(1).await;
}

#[tokio::test(start_paused = true)]
async fn max_nudges_then_cancel_and_blocked() {
    let mut fixture = Fixture::new(1).await;
    fixture.tick().await;
    fixture.wait_for_nudge(1).await;
    fixture.tick().await;
    let goal_id = fixture.goal_id.clone();
    fixture
        .wait_for(|event| {
            matches!(event,
                OrchestratorEvent::GoalStateChanged { goal_id: changed, to: GoalState::Blocked, .. }
                    if changed == &goal_id
            )
        })
        .await;

    assert_eq!(
        fixture
            .handle
            .snapshot(&fixture.goal_id)
            .expect("snapshot")
            .state,
        GoalState::Blocked
    );
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.expect("wait"),
        AgentRunPhase::Error
    );
}

#[tokio::test(start_paused = true)]
async fn repeated_tool_errors_trigger_stall() {
    let mut fixture = Fixture::new(2).await;
    for index in 0..3 {
        fixture.bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: format!("c{index}"),
            is_error: true,
            detail: None,
            output: None,
            run_id: Some(fixture.child.to_string()),
        }));
    }
    fixture.tick().await;
    let child = fixture.child.to_string();
    fixture
        .wait_for(|event| {
            matches!(
                event,
                OrchestratorEvent::StallDetected {
                    run_id,
                    signal: StallSignal::RepeatedErrors { count: 3 },
                    ..
                } if run_id == &child
            )
        })
        .await;
}

#[test]
fn supervisor_delivery_contract_has_no_worktree_mutation() {
    let contract = ShellCommandContract::delivery();
    for args in [
        vec!["add".to_string(), ".".to_string()],
        vec!["commit".to_string(), "-m".to_string(), "x".to_string()],
        vec!["checkout".to_string(), "main".to_string()],
        vec!["reset".to_string(), "--hard".to_string()],
    ] {
        assert!(matches!(
            contract.evaluate("git", &args),
            tools::CommandVerdict::Deny { .. }
        ));
    }
}
