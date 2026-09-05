mod support;

use std::sync::Arc;

use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, GoalState, LifecycleEvent, OrchestratorEvent,
    RunPurpose, StallSignal, ToolEvent,
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
    events: Arc<std::sync::Mutex<Vec<OrchestratorEvent>>>,
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
        let mut settings = OrchestrationSettings::default();
        settings.stall_after_secs = 0;
        settings.stall_check_secs = 1;
        settings.in_flight_tool_multiplier = 3;
        settings.repeated_error_threshold = 3;
        settings.max_nudges = max_nudges;
        let handle = GoalSupervisor::spawn(
            runtime.clone(),
            Arc::clone(&bus),
            Arc::new(FixtureDeliveryAdapter::default()),
            settings,
        );
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let mut subscriber = handle.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = subscriber.recv().await {
                if let EventKind::Orchestrator(event) = event.kind {
                    captured
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(event);
                }
            }
        });
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
        let fixture = Self {
            runtime,
            bus,
            handle,
            events,
            parent,
            child,
            goal_id,
        };
        fixture.settle().await;
        fixture
    }

    async fn settle(&self) {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }
    fn events(&self) -> Vec<OrchestratorEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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

#[tokio::test]
async fn no_progress_after_stall_window_sends_steering_nudge_from_parent() {
    let fixture = Fixture::new(2).await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    fixture.settle().await;

    assert!(fixture.events().iter().any(|event| matches!(event,
        OrchestratorEvent::NudgeSent { run_id, nudge_index: 1, .. } if run_id == &fixture.child.to_string()
    )));
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
    let mut settings = OrchestrationSettings::default();
    settings.stall_after_secs = 10;
    settings.in_flight_tool_multiplier = 3;
    assert_eq!(
        judge(&track, now + std::time::Duration::from_secs(20), &settings),
        None
    );
    assert_eq!(
        judge(&track, now + std::time::Duration::from_secs(31), &settings),
        Some(StallSignal::NoProgress)
    );
}

#[tokio::test]
async fn progress_resets_nudge_counter() {
    let fixture = Fixture::new(2).await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    fixture.settle().await;
    fixture.progress();
    fixture.settle().await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    fixture.settle().await;

    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| matches!(event,
                OrchestratorEvent::NudgeSent { run_id, nudge_index: 1, .. }
                    if run_id == &fixture.child.to_string()
            ))
            .count(),
        2
    );
}

#[tokio::test]
async fn max_nudges_then_cancel_and_blocked() {
    let fixture = Fixture::new(1).await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    fixture.settle().await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    fixture.settle().await;

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

#[tokio::test]
async fn repeated_tool_errors_trigger_stall() {
    let fixture = Fixture::new(2).await;
    for index in 0..3 {
        fixture.bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: format!("c{index}"),
            is_error: true,
            detail: None,
            run_id: Some(fixture.child.to_string()),
        }));
    }
    fixture.settle().await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    fixture.settle().await;

    assert!(fixture.events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::StallDetected {
            signal: StallSignal::RepeatedErrors { count: 3 },
            ..
        }
    )));
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
