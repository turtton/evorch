mod support;

#[path = "support/budget_supervisor.rs"]
mod budget_supervisor;

#[path = "support/durable_continuation.rs"]
mod durable_continuation;

#[path = "support/stale_worker.rs"]
mod stale_worker;

#[tokio::test]
async fn stale_worker_transitions_to_retrying_with_fresh_run_id() {
    stale_worker::transitions_to_retrying().await;
}

#[tokio::test]
async fn resume_task_after_interruption_replays_from_persisted_cursor() {
    durable_continuation::resume_after_interruption().await;
}

#[tokio::test]
async fn retry_till_attempt_cap_then_suppress() {
    durable_continuation::retry_to_cap().await;
}

#[tokio::test]
async fn cancel_task_persists_cancelled_status() {
    durable_continuation::cancel_persisted().await;
}

use std::sync::Arc;

use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, GoalStage, GoalState, LifecycleEvent,
    OrchestratorEvent, RunPurpose, SuppressReason,
};
use providers::FinishReason;
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::OrchestrationSettings;
use runtime::orchestration::supervisor::{GoalSpec, GoalSupervisor};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tools::ToolExecutor;

use support::ScriptedModel;

struct Fixture {
    model: Arc<ScriptedModel>,
    model_gate: Arc<Notify>,
    delivery: Arc<FixtureDeliveryAdapter>,
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
        let model_gate = Arc::new(Notify::new());
        let model = Arc::new(ScriptedModel::gated([], Arc::clone(&model_gate)));
        let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model.clone());
        let delivery = Arc::new(FixtureDeliveryAdapter::default());
        let settings = OrchestrationSettings {
            max_continuations,
            stall_after_secs: 86_400,
            stall_check_secs: 1,
            ..OrchestrationSettings::default()
        };
        let handle = GoalSupervisor::spawn(
            runtime.clone(),
            Arc::clone(&bus),
            delivery.clone(),
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
            model,
            model_gate,
            delivery,
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

    async fn terminal(&self, run_id: runtime::RunId) {
        let phase = self.runtime.inspect_agent(run_id).expect("run").phase;
        if matches!(
            phase,
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
        ) {
            // The supervisor consults runtime liveness, so a terminal fixture
            // must end the gated execution rather than only spoof a bus event.
            self.runtime.cancel(run_id).expect("cancel gated run");
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(2), self.runtime.wait(run_id))
                    .await
                    .expect("terminal timeout")
                    .expect("terminal phase"),
                AgentRunPhase::Error
            );
        } else {
            // Repeated calls exercise duplicate terminal-event handling.
            self.bus
                .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
                    run_id: run_id.to_string(),
                    from: AgentRunPhase::Running,
                    to: phase,
                    reason: None,
                }));
        }
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

    fixture.terminal(fixture.root).await;
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
async fn agent_summaries_expose_parent_run_ids() {
    // Given: a registered root run and a child run
    let fixture = Fixture::new(8).await;
    let child = fixture
        .runtime
        .delegate_background_as_child(fixture.root, Role::Worker, "CHILD", RunConfig::default())
        .expect("child run registration");
    fixture.settle().await;

    // When: the runtime summaries are listed
    let summaries = fixture.runtime.list_agents();

    // Then: the root has no parent and the child points to the root
    assert_eq!(
        summaries
            .iter()
            .find(|summary| summary.run_id == fixture.root)
            .and_then(|summary| summary.parent_run_id),
        None
    );
    assert_eq!(
        summaries
            .iter()
            .find(|summary| summary.run_id == child)
            .and_then(|summary| summary.parent_run_id),
        Some(fixture.root)
    );
}

#[tokio::test]
async fn duplicate_terminal_event_is_suppressed_as_duplicate() {
    let fixture = Fixture::new(8).await;
    fixture.terminal(fixture.root).await;
    fixture.settle().await;

    fixture.terminal(fixture.root).await;
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

    fixture.terminal(fixture.root).await;
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
async fn paused_goal_records_worker_done_without_delivery_or_continuation() {
    let fixture = Fixture::new(8).await;
    fixture
        .model
        .add_keyed(
            "IMPL",
            [Ok(support::text_response(
                "implemented",
                FinishReason::Stop,
            ))],
        )
        .await;
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
    // Supply a branch without a worktree so delivery would not return early.
    // Only events after setup are checked for new delivery side effects.
    fixture
        .bus
        .emit(Event::new(OrchestratorEvent::DeliverableBranchBound {
            goal_id: fixture.goal_id.clone(),
            branch: "feature/paused-worker".into(),
            run_id: child.to_string(),
        }));
    fixture.settle().await;

    fixture
        .handle
        .pause(&fixture.goal_id)
        .expect("operator pause");
    fixture.settle().await;
    fixture.terminal(fixture.root).await;
    fixture.settle().await;
    let paused = fixture.handle.snapshot(&fixture.goal_id).expect("snapshot");
    assert_eq!(paused.state, GoalState::Paused);
    assert_eq!(paused.stage, GoalStage::Implementing);
    assert_eq!(
        paused.deliverable_branch.as_deref(),
        Some("feature/paused-worker")
    );
    assert!(
        paused
            .attached_runs
            .iter()
            .any(|run| { run.run_id == child.to_string() && run.purpose == RunPurpose::Implement })
    );
    assert!(matches!(
        fixture.runtime.inspect_agent(child).expect("child").phase,
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
    ));
    let orchestrators_before = paused
        .attached_runs
        .iter()
        .filter(|run| run.role == Role::Orchestrator.name())
        .count();
    let events_before = fixture.orchestrator_events().len();
    assert!(fixture.delivery.recorded().is_empty());

    // The root has stopped, but its surviving worker completes successfully.
    fixture.model_gate.notify_one();
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.runtime.wait(child)
        )
        .await
        .expect("worker completion timeout")
        .expect("worker phase"),
        AgentRunPhase::Done
    );
    fixture.settle().await;

    let events = fixture.orchestrator_events();
    assert!(
        !events[events_before..]
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::DeliverableBranchBound { .. }))
    );
    assert!(fixture.delivery.recorded().is_empty());
    let completed = fixture.handle.snapshot(&fixture.goal_id).expect("snapshot");
    assert_eq!(completed.state, GoalState::Paused);
    assert_eq!(completed.stage, paused.stage);
    assert_eq!(
        completed
            .attached_runs
            .iter()
            .filter(|run| run.role == Role::Orchestrator.name())
            .count(),
        orchestrators_before
    );
    assert_eq!(completed.attached_runs, paused.attached_runs);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::ContinuationDispatched { .. }))
    );
}

#[tokio::test]
async fn resume_with_live_current_orchestrator_does_not_dispatch_duplicate() {
    // Given: the current orchestrator remains live while its goal is paused.
    let fixture = Fixture::new(8).await;
    fixture
        .handle
        .pause(&fixture.goal_id)
        .expect("pause command");
    fixture.settle().await;
    let paused = fixture.handle.snapshot(&fixture.goal_id).expect("snapshot");
    assert_eq!(paused.state, GoalState::Paused);
    assert!(matches!(
        fixture
            .runtime
            .inspect_agent(fixture.root)
            .expect("root")
            .phase,
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
    ));
    let orchestrators_before = fixture
        .runtime
        .list_agents()
        .into_iter()
        .filter(|run| run.role_name == Role::Orchestrator.name())
        .count();

    // When: resume advances the epoch and invokes try_dispatch.
    fixture
        .handle
        .resume(&fixture.goal_id)
        .expect("resume command");
    fixture.settle().await;

    // Then: epoch > 0 must not override the current run's live registration.
    let resumed = fixture.handle.snapshot(&fixture.goal_id).expect("snapshot");
    assert_eq!(resumed.state, GoalState::Active);
    assert_eq!(resumed.epoch, paused.epoch + 1);
    assert_eq!(
        resumed.current_orchestrator_run_id,
        fixture.root.to_string()
    );
    assert_eq!(resumed.attached_runs, paused.attached_runs);
    assert!(resumed.dispatched_epochs.is_empty());
    assert_eq!(
        fixture
            .runtime
            .list_agents()
            .into_iter()
            .filter(|run| run.role_name == Role::Orchestrator.name())
            .count(),
        orchestrators_before
    );
    assert!(
        !fixture
            .orchestrator_events()
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::ContinuationDispatched { .. }))
    );
}

#[tokio::test]
async fn message_delta_and_timer_advance_never_dispatch() {
    let fixture = Fixture::new(8).await;
    fixture
        .bus
        .emit(Event::new(event_bus::MessageEvent::MessageDelta {
            delta: "done".into(),
            run_id: None,
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
    blocked.terminal(blocked.root).await;
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
    cancelled.terminal(cancelled.root).await;
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

    fixture.terminal(fixture.root).await;
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

    fixture.terminal(fixture.root).await;
    fixture.settle().await;
    assert!(fixture.orchestrator_events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ContinuationSuppressed {
            reason: SuppressReason::PipelineBusy,
            ..
        }
    )));

    fixture.terminal(child).await;
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

    fixture.terminal(fixture.root).await;
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

    fixture.terminal(child).await;
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
