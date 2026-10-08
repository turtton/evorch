use super::*;
use crate::AgentRuntime;
use crate::orchestration::{GoalGate, gate::GateVerdict};
use std::future::Future;
use std::pin::Pin;

struct BlockedModel;

#[async_trait::async_trait]
impl AgentModel for BlockedModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "blocked".into()
    }
}

struct InterruptOnAttach(watch::Sender<RunInterrupt>, RunInterrupt);

impl GoalGate for InterruptOnAttach {
    fn attach_child(&self, _: RunId, _: RunId, _: Role) {
        self.0.send_replace(self.1);
    }

    fn evaluate_finish<'a>(
        &'a self,
        _: RunId,
    ) -> Pin<Box<dyn Future<Output = Option<GateVerdict>> + Send + 'a>> {
        Box::pin(async { None })
    }
}

fn fixture(cancel_rx: watch::Receiver<RunInterrupt>) -> (AgentRuntime, LoopState) {
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(BlockedModel),
    );
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let mailbox = Arc::new(RunMailbox::new());
    let state = LoopState {
        benchmark: None,
        benchmark_checked: false,
        benchmark_replay: false,
        task: RunTask {
            run_id: parent,
            role: Role::Orchestrator,
            prompt: "parent".into(),
            start: RunStart::WithInput,
            config: RunConfig::default(),
            parent: None,
            mailbox: mailbox.clone(),
            handoff: None,
            restored: None,
        },
        shared: loop_shared(&Arc::downgrade(&runtime.shared), None).expect("runtime"),
        channels: LoopChannels {
            phase_tx: watch::channel(AgentRunPhase::Pending).0,
            message_count_tx: watch::channel(0).0,
            inbox_rx: mpsc::channel(1).1,
            user_inbox: Arc::new(crate::runtime::user_inbox::UserInbox::default()),
            cancel_rx,
            mailbox_version_rx: mailbox.subscribe_version(),
            compact_rx: watch::channel(0).1,
            model_preference_rx: watch::channel(None).1,
            compaction_busy: Arc::new(AtomicBool::new(false)),
            result_tx: watch::channel(None).0,
        },
        run_state: RunState::new(),
        context: AgentContext::new(parent, Role::Orchestrator),
        policy: runtime.execution_policy(Role::Orchestrator),
        tool_specs: Vec::new(),
        rules_session: None,
        compaction: CompactionLoopState::default(),
        last_usage: None,
        answered_questions: Default::default(),
        resumed: false,
        completed_turn_end: None,
        pending_user_messages: Vec::new(),
        goal_wake_pending: false,
        todo_context_pending: false,
        todo_context_deferred: false,
        pending_escalation: None,
        escalation_detector: EscalationDetector::default(),
        budget: crate::budget_tracker::BudgetCounters::default(),
        identical_calls: identical_calls::IdenticalCalls::default(),
        durable_task: None,
        pending_terminal: None,
    };
    (runtime, state)
}

#[tokio::test]
async fn cancellation_between_delegate_spawns_drains_first_child() {
    // Given: the synchronous attach callback cancels after the first spawn, before the second check.
    let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(cancel_rx);
    let runtime = runtime.with_goal_gate(Arc::new(InterruptOnAttach(cancel, RunInterrupt::Cancel)));
    state
        .transition(AgentRunPhase::Running, None)
        .expect("running");
    // When: execute a two-delegate wave without yielding at the attach seam.
    assert!(
        !state
            .execute_tools(
                vec![
                    (
                        "first".into(),
                        "delegate".into(),
                        serde_json::json!({"target": {"role": "worker"},"prompt":"first"})
                    ),
                    (
                        "second".into(),
                        "delegate".into(),
                        serde_json::json!({"target": {"role": "worker"},"prompt":"second"})
                    ),
                ],
                &crate::AgentInvocationContext::default(),
                &BlockedModel
            )
            .await
    );
    // Then: returning from the wave already implies child termination, not merely cancellation requested.
    let agents = runtime.list_agents();
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[1].phase, AgentRunPhase::Error);
    runtime
        .cancel(state.caller_run_id())
        .expect("stop fixture parent");
    runtime
        .wait(state.caller_run_id())
        .await
        .expect("parent stopped");
}

#[tokio::test]
async fn rejected_waiting_transition_drains_all_spawned_children() {
    // Given: a Pending LoopState rejects Waiting; real children use the existing spawn path.
    let (_cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(cancel_rx);
    let first = crate::meta::spawn_delegate(
        &mut state,
        &runtime,
        serde_json::json!({"target": {"role": "worker"},"prompt":"first"}),
    );
    let second = crate::meta::spawn_delegate(
        &mut state,
        &runtime,
        serde_json::json!({"target": {"role": "worker"},"prompt":"second"}),
    );
    assert!(first.is_ok() && second.is_ok());
    // When: the parent cannot enter Waiting, with a pre-existing per-call rejection interleaved.
    let results = crate::meta::wait_delegates(
        &mut state,
        &runtime,
        vec![
            first,
            Err(crate::meta::DispatchResult {
                result: tools::ToolResult::error("invalid child"),
                terminal: crate::meta::Terminal::Continue,
            }),
            second,
        ],
    )
    .await;
    // Then: both children are terminal on return and the original error order is retained.
    assert_eq!(
        results
            .iter()
            .map(|result| (result.result.content.as_str(), result.result.is_error))
            .collect::<Vec<_>>(),
        [
            ("parent run could not enter Waiting", true),
            ("invalid child", true),
            ("parent run could not enter Waiting", true),
        ]
    );
    let agents = runtime.list_agents();
    assert_eq!(agents.len(), 3);
    assert!(
        agents[1..]
            .iter()
            .all(|child| child.phase == AgentRunPhase::Error),
        "children must be drained before returning: {agents:?}"
    );
    runtime
        .cancel(state.caller_run_id())
        .expect("stop fixture parent");
    runtime
        .wait(state.caller_run_id())
        .await
        .expect("parent stopped");
}

#[tokio::test]
async fn stop_between_delegate_spawns_preserves_first_child_and_defers_terminal_publication() {
    let (interrupt, interrupt_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(interrupt_rx);
    let runtime =
        runtime.with_goal_gate(Arc::new(InterruptOnAttach(interrupt, RunInterrupt::Stop)));
    state.transition(AgentRunPhase::Running, None).unwrap();
    assert!(
        !state
            .execute_tools(
                vec![
                    (
                        "first".into(),
                        "delegate".into(),
                        serde_json::json!({"target": {"role": "worker"},"prompt":"first"})
                    ),
                    (
                        "second".into(),
                        "delegate".into(),
                        serde_json::json!({"target": {"role": "worker"},"prompt":"second"})
                    ),
                ],
                &crate::AgentInvocationContext::default(),
                &BlockedModel
            )
            .await
    );
    let agents = runtime.list_agents();
    assert_eq!(agents.len(), 2);
    assert!(matches!(
        agents[1].phase,
        AgentRunPhase::Pending | AgentRunPhase::Running
    ));
    assert_eq!(state.run_state.phase(), AgentRunPhase::Stopped);
    assert_eq!(*state.channels.phase_tx.borrow(), AgentRunPhase::Running);
    assert_eq!(*state.channels.result_tx.borrow(), None);
    assert!(
        matches!(state.pending_terminal.as_ref(), Some((_, LifecycleEvent::AgentRunStateChanged {
        to: AgentRunPhase::Stopped, reason: Some(reason), ..
    })) if reason == "stopped")
    );
    // The test LoopState is independent of the fixture runtime's real parent loop.
    for run in agents {
        runtime.cancel(run.run_id).unwrap();
        runtime.wait(run.run_id).await.unwrap();
    }
}

#[tokio::test]
async fn stopped_wait_keeps_awaited_child_and_preserves_wait_error_contract() {
    let (interrupt, interrupt_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(interrupt_rx);
    state.transition(AgentRunPhase::Running, None).unwrap();
    let child = crate::meta::spawn_delegate(
        &mut state,
        &runtime,
        serde_json::json!({"target": {"role": "worker"},"prompt":"child"}),
    )
    .ok()
    .unwrap();
    interrupt.send_replace(RunInterrupt::Stop);
    let results = crate::meta::wait_delegates(&mut state, &runtime, vec![Ok(child)]).await;
    assert_eq!(results.len(), 1);
    assert!(results[0].result.is_error);
    assert_eq!(results[0].result.content, "wait cancelled");
    assert!(matches!(
        runtime.inspect_agent(child).unwrap().phase,
        AgentRunPhase::Pending | AgentRunPhase::Running
    ));
    for run in [child, state.caller_run_id()] {
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}

#[tokio::test]
async fn goal_resume_after_paused_boundary_survives_post_boundary_inbox_drain() {
    let (_, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = fixture(cancel_rx);
    let root = state.task.run_id;
    runtime.bind_thread_root("resume-race", root).unwrap();
    let goal = runtime
        .create_thread_goal("resume-race", root, "Work".into(), vec!["Evidence".into()])
        .unwrap();
    runtime
        .set_goal_checks_paused("resume-race", &goal.goal_id, true)
        .unwrap();
    state.transition(AgentRunPhase::Running, None).unwrap();
    // Precisely interleave Resume between boundary=false and the post-boundary
    // flush used by both natural Stop and explicit finish, without a timed race.
    assert!(!state.thread_goal_boundary().await);
    runtime
        .set_goal_checks_paused("resume-race", &goal.goal_id, false)
        .unwrap();
    state.queue_user_message((crate::thread_goals::CHECKS_WAKE.into(), Vec::new(), false));
    assert!(!state.flush_aside());
    assert!(state.goal_wake_pending);
    assert!(
        state.wait_for_input().await,
        "the root must check before parking"
    );
    assert_eq!(
        runtime.thread_goal("resume-race").unwrap().phase,
        event_bus::ThreadGoalPhase::Checking
    );
    assert!(!state.goal_wake_pending);
    runtime.cancel(root).unwrap();
    runtime.wait(root).await.unwrap();
}
