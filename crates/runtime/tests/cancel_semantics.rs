mod support;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventBus, EventKind, GoalState, OrchestratorEvent};
use providers::{ChatResponse, Message, ToolSpec};
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::OrchestrationSettings;
use runtime::orchestration::supervisor::{GoalSpec, GoalSupervisor, SupervisorHandle};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError, WorkspaceMode,
};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tools::ToolExecutor;

fn fixture(model: Arc<dyn AgentModel>) -> (AgentRuntime, SupervisorHandle, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(512));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus.clone(), executor, model);
    let handle = GoalSupervisor::spawn(
        runtime.clone(),
        bus.clone(),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    (runtime, handle, bus)
}

fn spec() -> GoalSpec {
    GoalSpec {
        session_id: "s".into(),
        project_id: "p".into(),
        thread_id: "t".into(),
        goal: "cancel regression".into(),
        references: vec![],
        constraints: vec![],
        repo: "fixture/repo".into(),
        base_ref: "main".into(),
    }
}

#[tokio::test]
async fn isolated_without_workspace_context_is_terminal_not_retried() {
    // Given: an actual isolated run without the workspace composition seam.
    let (runtime, handle, bus) = fixture(Arc::new(support::ScriptedModel::gated(
        [],
        Arc::new(Notify::new()),
    )));
    let mut events = bus.subscribe();
    let run = runtime.reserve_run_id();
    let goal = handle.create_goal(spec(), run);
    // When: workspace initialization fails.
    runtime.spawn_reserved(
        run,
        None,
        Role::Orchestrator,
        "isolated",
        RunConfig {
            workspace_mode: WorkspaceMode::Isolated,
            ..RunConfig::default()
        },
    );
    // Then: the goal blocks without creating any retry generation.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await.expect("event").kind {
                EventKind::Orchestrator(OrchestratorEvent::ContinuationDispatched { .. }) => {
                    panic!("configuration failure retried")
                }
                EventKind::Orchestrator(OrchestratorEvent::GoalStateChanged {
                    to: GoalState::Blocked,
                    ..
                }) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("configuration failure must block");
    assert_eq!(
        handle.snapshot(&goal).expect("goal").state,
        GoalState::Blocked
    );
    assert_eq!(runtime.list_agents().len(), 1);
}

#[tokio::test]
async fn cancel_before_retry_dispatch_suppresses_respawn() {
    // Given: a goal whose pending root can produce a terminal event immediately.
    let (runtime, handle, _) = fixture(Arc::new(support::ScriptedModel::gated(
        [],
        Arc::new(Notify::new()),
    )));
    let root = runtime.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
    let goal = handle.create_goal(spec(), root);
    // When: cancellation is accepted before the actor consumes the terminal event.
    handle.cancel(&goal).expect("cancel");
    // Then: the cancellation fence is already visible, not merely queued.
    assert_eq!(
        handle.snapshot(&goal).expect("goal").state,
        GoalState::Cancelled
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), runtime.wait(root))
            .await
            .expect("timeout")
            .expect("terminal"),
        AgentRunPhase::Error
    );
    assert!(
        handle
            .snapshot(&goal)
            .expect("goal")
            .dispatched_epochs
            .is_empty()
    );
    let late = runtime.spawn_reserved(
        runtime.reserve_run_id(),
        Some(root),
        Role::Explorer,
        "late child",
        RunConfig::default(),
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), runtime.wait(late))
            .await
            .expect("late child timeout")
            .expect("late child"),
        AgentRunPhase::Error
    );
}

#[test]
fn durable_configuration_failure_is_not_dispatchable() {
    // Given: persisted failed work whose configuration cannot change on retry.
    let task = serde_json::from_value(serde_json::json!({
        "status": "failed", "input": "work", "attempts": 0,
        "failure_reason": "workspace isolation requires workspace context"
    }))
    .expect("durable state");
    // When: the continuation policy evaluates the task below its attempt cap.
    let decision = runtime::orchestration::continuation::decide_task(&task, 10);
    // Then: the configuration failure blocks dispatch, rather than consuming retries.
    assert_eq!(
        decision,
        runtime::orchestration::continuation::ContinuationDecision::Suppress(
            event_bus::SuppressReason::Blocked
        )
    );
}

struct AdmissionModel {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl AgentModel for AdmissionModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "admission-test".into()
    }
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancel_covers_pending_and_admission_runs() {
    // Given: a goal root and child reserved but not yet admitted (no AgentRunStarted).
    let model = Arc::new(AdmissionModel {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let (runtime, handle, bus) = fixture(model.clone());
    let root = runtime.reserve_run_id();
    let goal = handle.create_goal(spec(), root);
    runtime.spawn_reserved(root, None, Role::Orchestrator, "root", RunConfig::default());
    model.entered.notified().await;
    let child = runtime.reserve_run_id();
    runtime.spawn_reserved(
        child,
        Some(root),
        Role::Explorer,
        "child",
        RunConfig::default(),
    );
    model.entered.notified().await;
    let mut events = bus.subscribe();
    // When: cancel is processed while both admissions are suspended.
    handle.cancel(&goal).expect("cancel");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                events.recv().await.expect("event").kind,
                EventKind::Orchestrator(OrchestratorEvent::GoalStateChanged {
                    to: GoalState::Cancelled,
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .expect("cancelled event");
    model.release.notify_waiters();
    // Then: neither admission may register a run after cancellation.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), runtime.wait(root))
            .await
            .expect("root admission must terminate")
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(1), runtime.wait(child))
            .await
            .expect("child admission must terminate")
            .is_err()
    );
    assert!(runtime.list_agents().is_empty());
}
