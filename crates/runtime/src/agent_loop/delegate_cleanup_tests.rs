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

struct CancelOnAttach(watch::Sender<bool>);

impl GoalGate for CancelOnAttach {
    fn attach_child(&self, _: RunId, _: RunId, _: Role) {
        self.0.send_replace(true);
    }

    fn evaluate_finish<'a>(
        &'a self,
        _: RunId,
    ) -> Pin<Box<dyn Future<Output = Option<GateVerdict>> + Send + 'a>> {
        Box::pin(async { None })
    }
}

fn fixture(cancel_rx: watch::Receiver<bool>) -> (AgentRuntime, LoopState) {
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
        task: RunTask {
            run_id: parent,
            role: Role::Orchestrator,
            prompt: "parent".into(),
            config: RunConfig::default(),
            parent: None,
            mailbox: mailbox.clone(),
            handoff: None,
            restored: None,
        },
        shared: loop_shared(&Arc::downgrade(&runtime.shared)).expect("runtime"),
        channels: LoopChannels {
            phase_tx: watch::channel(AgentRunPhase::Pending).0,
            message_count_tx: watch::channel(0).0,
            inbox_rx: mpsc::channel(1).1,
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
    let (cancel, cancel_rx) = watch::channel(false);
    let (runtime, mut state) = fixture(cancel_rx);
    let runtime = runtime.with_goal_gate(Arc::new(CancelOnAttach(cancel)));
    state
        .transition(AgentRunPhase::Running, None)
        .expect("running");
    // When: execute a two-delegate wave without yielding at the attach seam.
    assert!(
        !state
            .execute_tools(vec![
                (
                    "first".into(),
                    "delegate".into(),
                    serde_json::json!({"prompt":"first"})
                ),
                (
                    "second".into(),
                    "delegate".into(),
                    serde_json::json!({"prompt":"second"})
                ),
            ])
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
    let (_cancel, cancel_rx) = watch::channel(false);
    let (runtime, mut state) = fixture(cancel_rx);
    let first =
        crate::meta::spawn_delegate(&mut state, &runtime, serde_json::json!({"prompt":"first"}));
    let second =
        crate::meta::spawn_delegate(&mut state, &runtime, serde_json::json!({"prompt":"second"}));
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
