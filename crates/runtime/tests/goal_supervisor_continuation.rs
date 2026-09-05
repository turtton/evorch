mod support;

use std::sync::Arc;

use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, GoalState, LifecycleEvent, OrchestratorEvent,
    RunPurpose, SuppressReason,
};
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::OrchestrationSettings;
use runtime::orchestration::supervisor::{GoalSpec, GoalSupervisor};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tools::ToolExecutor;

use support::ScriptedModel;

struct Fixture {
    runtime: AgentRuntime,
    bus: Arc<EventBus>,
    handle: runtime::orchestration::supervisor::SupervisorHandle,
    events: Arc<std::sync::Mutex<Vec<OrchestratorEvent>>>,
    root: runtime::RunId,
    goal_id: String,
}

impl Fixture {
    async fn new(max_continuations: u32) -> Self {
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
            max_continuations,
            stall_after_secs: 86_400,
            ..OrchestrationSettings::default()
        };
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
        let root = runtime.delegate_background(
            Role::Orchestrator,
            "ROOT".to_string(),
            RunConfig::default(),
        );
        let goal_id = handle.create_goal(
            GoalSpec {
                session_id: "session-1".into(),
                project_id: "evorch".into(),
                thread_id: "thread-1".into(),
                goal: "finish issue 73".into(),
                references: vec![],
                constraints: vec![],
                repo: "turtton/evorch".into(),
                base_ref: "main".into(),
            },
            root,
        );
        let fixture = Self {
            runtime,
            bus,
            handle,
            events,
            root,
            goal_id,
        };
        fixture.settle().await;
        fixture
    }

    fn terminal(&self, run_id: runtime::RunId) {
        self.bus
            .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: run_id.to_string(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            }));
    }

    async fn settle(&self) {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    fn orchestrator_events(&self) -> Vec<OrchestratorEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[tokio::test]
async fn continuation_dispatched_exactly_once_per_terminal_epoch() {
    let fixture = Fixture::new(8).await;

    fixture.terminal(fixture.root);
    fixture.settle().await;

    let dispatched = fixture
        .orchestrator_events()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                OrchestratorEvent::ContinuationDispatched { epoch: 1, .. }
            )
        })
        .count();
    assert_eq!(dispatched, 1);
    assert!(
        fixture
            .handle
            .snapshot(&fixture.goal_id)
            .is_some_and(|s| s.dispatched_epochs.contains(&1))
    );
}

#[tokio::test]
async fn duplicate_terminal_event_is_suppressed_as_duplicate() {
    let fixture = Fixture::new(8).await;
    fixture.terminal(fixture.root);
    fixture.settle().await;

    fixture.terminal(fixture.root);
    fixture.settle().await;

    assert!(fixture.orchestrator_events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ContinuationSuppressed {
            epoch: 1,
            reason: SuppressReason::Duplicate,
            ..
        }
    )));
}

#[tokio::test]
async fn paused_goal_suppresses_and_resume_dispatches_new_epoch() {
    let fixture = Fixture::new(8).await;
    fixture
        .handle
        .pause(&fixture.goal_id)
        .expect("pause command");
    fixture.settle().await;

    fixture.terminal(fixture.root);
    fixture.settle().await;
    assert!(fixture.orchestrator_events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ContinuationSuppressed {
            reason: SuppressReason::Paused,
            ..
        }
    )));

    fixture
        .handle
        .resume(&fixture.goal_id)
        .expect("resume command");
    fixture.settle().await;
    assert!(fixture.orchestrator_events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ContinuationDispatched { epoch: 2, .. }
    )));
}

#[tokio::test]
async fn message_delta_and_timer_advance_never_dispatch() {
    let fixture = Fixture::new(8).await;
    fixture
        .bus
        .emit(Event::new(event_bus::MessageEvent::MessageDelta {
            delta: "done".into(),
        }));
    fixture.settle().await;

    assert!(
        !fixture
            .orchestrator_events()
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::ContinuationDispatched { .. }))
    );
}

#[tokio::test]
async fn blocked_and_complete_never_dispatch() {
    let blocked = Fixture::new(8).await;
    blocked
        .bus
        .emit(Event::new(OrchestratorEvent::GoalStateChanged {
            goal_id: blocked.goal_id.clone(),
            from: GoalState::Active,
            to: GoalState::Blocked,
            reason: "blocked by test".into(),
        }));
    blocked.settle().await;
    blocked.terminal(blocked.root);
    blocked.settle().await;
    assert!(
        !blocked
            .orchestrator_events()
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::ContinuationDispatched { .. }))
    );

    let cancelled = Fixture::new(8).await;
    cancelled.handle.cancel(&cancelled.goal_id).expect("cancel");
    cancelled.settle().await;
    cancelled.terminal(cancelled.root);
    cancelled.settle().await;
    assert!(
        !cancelled
            .orchestrator_events()
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::ContinuationDispatched { .. }))
    );
}

#[tokio::test]
async fn limit_reached_blocks_goal() {
    let fixture = Fixture::new(0).await;

    fixture.terminal(fixture.root);
    fixture.settle().await;

    assert_eq!(
        fixture
            .handle
            .snapshot(&fixture.goal_id)
            .expect("snapshot")
            .state,
        GoalState::Blocked
    );
    assert!(fixture.orchestrator_events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ContinuationSuppressed {
            reason: SuppressReason::LimitReached { max: 0 },
            ..
        }
    )));
}

#[tokio::test]
async fn dispatch_deferred_while_pipeline_busy_then_fires_once() {
    let fixture = Fixture::new(8).await;
    let child = fixture
        .runtime
        .delegate_background_as_child(fixture.root, Role::Reviewer, "REVIEW", RunConfig::default())
        .expect("review child");
    fixture.bus.emit(Event::new(OrchestratorEvent::RunAttached {
        goal_id: fixture.goal_id.clone(),
        run_id: child.to_string(),
        parent_run_id: Some(fixture.root.to_string()),
        role: "reviewer".into(),
        purpose: RunPurpose::Review { round: 1 },
    }));
    fixture.settle().await;

    fixture.terminal(fixture.root);
    fixture.settle().await;
    assert!(fixture.orchestrator_events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ContinuationSuppressed {
            reason: SuppressReason::PipelineBusy,
            ..
        }
    )));

    fixture.terminal(child);
    fixture.settle().await;
    assert_eq!(
        fixture
            .orchestrator_events()
            .iter()
            .filter(|event| matches!(
                event,
                OrchestratorEvent::ContinuationDispatched { epoch: 1, .. }
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn dispatch_stays_deferred_while_implement_worker_is_alive() {
    let fixture = Fixture::new(8).await;
    let child = fixture
        .runtime
        .delegate_background_as_child(fixture.root, Role::Worker, "IMPL", RunConfig::default())
        .expect("implement child");
    fixture.bus.emit(Event::new(OrchestratorEvent::RunAttached {
        goal_id: fixture.goal_id.clone(),
        run_id: child.to_string(),
        parent_run_id: Some(fixture.root.to_string()),
        role: "worker".into(),
        purpose: RunPurpose::Implement,
    }));
    fixture.settle().await;

    fixture.terminal(fixture.root);
    fixture.settle().await;
    assert!(fixture.orchestrator_events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ContinuationSuppressed {
            reason: SuppressReason::PipelineBusy,
            ..
        }
    )));

    // 別の run 状態変化で再チェックが走っても、Implement worker 稼働中は
    // dispatch されない (実バイナリで観測された cascade 回帰)。
    fixture
        .bus
        .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: child.to_string(),
            from: AgentRunPhase::Pending,
            to: AgentRunPhase::Running,
            reason: None,
        }));
    fixture.settle().await;
    assert_eq!(
        fixture
            .orchestrator_events()
            .iter()
            .filter(|event| matches!(event, OrchestratorEvent::ContinuationDispatched { .. }))
            .count(),
        0
    );

    fixture.terminal(child);
    fixture.settle().await;
    assert_eq!(
        fixture
            .orchestrator_events()
            .iter()
            .filter(|event| matches!(
                event,
                OrchestratorEvent::ContinuationDispatched { epoch: 1, .. }
            ))
            .count(),
        1
    );
}
