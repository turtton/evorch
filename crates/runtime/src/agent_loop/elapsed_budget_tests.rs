use super::super::*;
use crate::budget_tracker::{BudgetContext, BudgetDecision};
use event_bus::{AgentMessage, AgentMessageKind, EventKind, OrchestratorEvent};
use futures_util::poll;
use serde_json::json;
use std::time::Duration;

struct StartedModel(tokio::sync::mpsc::UnboundedSender<()>);

#[async_trait::async_trait]
impl AgentModel for StartedModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        self.0.send(()).unwrap();
        std::future::pending().await
    }

    fn requires_admission(&self) -> bool {
        false
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "blocked".into()
    }
}

async fn child_fixture() -> (watch::Sender<RunInterrupt>, crate::AgentRuntime, LoopState) {
    let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    let (_template_runtime, mut state) = super::super::delegate_cleanup_tests::fixture(cancel_rx);
    let bus = Arc::new(EventBus::new(128));
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let runtime = crate::AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(StartedModel(started)),
    );
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    starts.recv().await.unwrap();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    // Both live runs are inside the model before a test queues input or attaches
    // storage. Their startup drains cannot steal the test loop's input.
    starts.recv().await.unwrap();
    state.shared = loop_shared(&Arc::downgrade(&runtime.shared), None).unwrap();
    state.task.parent = Some(parent);
    state.task.run_id = child;
    state.task.role = Role::Worker;
    state.task.mailbox = runtime.run_mailbox(child).unwrap();
    state.channels.mailbox_version_rx = state.task.mailbox.subscribe_version();
    state.context = AgentContext::new(child, Role::Worker);
    state.budget = crate::budget_tracker::BudgetCounters::default();
    state.transition(AgentRunPhase::Running, None).unwrap();
    (cancel, runtime, state)
}

fn message(state: &LoopState, sender: RunId, kind: AgentMessageKind) -> AgentMessage {
    AgentMessage {
        message_id: "activity-reset".into(),
        sender_run_id: sender.to_string(),
        recipient_run_id: state.task.run_id.to_string(),
        kind,
        content: "new input".into(),
        reply_to: None,
    }
}

/// Check the public checkpoint contract without exposing the tracker's clock.
fn elapsed_ms(state: &mut LoopState, tool_calls: u32) -> u64 {
    let mut events = state.shared.bus.subscribe();
    let decision = state.budget.publish(
        tool_calls,
        &BudgetContext {
            bus: &state.shared.bus,
            run_id: &state.task.run_id.to_string(),
            task_id: "activity-reset",
            settings: &state.task.config.budget,
        },
    );
    assert_eq!(decision, BudgetDecision::Continue);
    events
        .drain_pending_snapshot()
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::Orchestrator(OrchestratorEvent::TaskCheckpoint { elapsed_ms, .. }) => {
                Some(elapsed_ms)
            }
            _ => None,
        })
        .expect("checkpoint")
}

#[tokio::test(start_paused = true)]
async fn running_parent_messages_reset_on_consumption_not_arrival() {
    for kind in [
        AgentMessageKind::Send,
        AgentMessageKind::Reply,
        AgentMessageKind::Steering,
    ] {
        let (_cancel, _runtime, mut state) = child_fixture().await;
        state.context.push_user("already sent prefix");
        let prefix = state.context.messages.clone();
        tokio::time::advance(Duration::from_secs(10)).await;
        state
            .task
            .mailbox
            .try_push(message(&state, state.task.parent.unwrap(), kind))
            .unwrap();
        assert_eq!(elapsed_ms(&mut state, 50), 10_000);
        tokio::time::advance(Duration::from_secs(5)).await;
        state.inject_parent_messages();
        assert_eq!(elapsed_ms(&mut state, 100), 0);
        assert_eq!(&state.context.messages[..prefix.len()], prefix);
        tokio::time::advance(Duration::from_secs(7)).await;
        state.inject_parent_messages();
        assert_eq!(elapsed_ms(&mut state, 150), 7_000);
    }
}

#[tokio::test(start_paused = true)]
async fn interactive_parent_turns_get_fresh_windows_and_still_exclude_waiting() {
    let (_cancel, _runtime, mut state) = child_fixture().await;
    let (_input, input_rx) = tokio::sync::mpsc::channel(1);
    state.channels.inbox_rx = input_rx.into();
    for checkpoint in [50, 100] {
        tokio::time::advance(Duration::from_secs(3_600)).await;
        let input = message(&state, state.task.parent.unwrap(), AgentMessageKind::Send);
        let mailbox = state.task.mailbox.clone();
        {
            let waiting = state.wait_for_input_boundary(true);
            tokio::pin!(waiting);
            assert!(poll!(&mut waiting).is_pending());
            tokio::time::advance(Duration::from_secs(8 * 3_600)).await;
            mailbox.try_push(input).unwrap();
            assert!(waiting.await);
        }
        assert_eq!(elapsed_ms(&mut state, checkpoint), 0);
    }
    tokio::time::advance(Duration::from_secs(7_200)).await;
    assert_eq!(state.publish_budget(), BudgetDecision::Continue);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(matches!(
        state.publish_budget(),
        BudgetDecision::Exhausted(_)
    ));
}

#[tokio::test(start_paused = true)]
async fn non_parent_messages_and_tool_results_do_not_renew_activity() {
    let (_cancel, _runtime, mut state) = child_fixture().await;
    let descendant = RunId::new(999);
    for (index, sender) in [state.task.run_id, descendant, RunId::new(998)]
        .into_iter()
        .enumerate()
    {
        tokio::time::advance(Duration::from_secs(10)).await;
        state
            .task
            .mailbox
            .try_push(message(&state, sender, AgentMessageKind::Send))
            .unwrap();
        assert!(state.flush_aside());
        assert_eq!(
            elapsed_ms(&mut state, (index as u32 + 1) * 50),
            (index as u64 + 1) * 10_000
        );
    }
    state
        .context
        .push_tool_result("tool", tools::ToolResult::success("result"));
    assert_eq!(elapsed_ms(&mut state, 200), 30_000);
}

#[tokio::test(start_paused = true)]
async fn trusted_child_input_is_parent_equivalent_but_internal_and_root_input_are_not() {
    let (_cancel, _runtime, mut state) = child_fixture().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(state.take_user_input(("internal".into(), vec![], false, None, 0)));
    assert_eq!(elapsed_ms(&mut state, 50), 10_000);
    assert!(state.take_user_input(("human".into(), vec![], true, None, 0)));
    assert_eq!(elapsed_ms(&mut state, 100), 0);
    state.task.parent = None;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(state.take_user_input(("root human".into(), vec![], true, None, 0)));
    assert_eq!(elapsed_ms(&mut state, 150), 10_000);
}

#[tokio::test(start_paused = true)]
async fn inbox_delivery_renews_only_when_a_parent_message_is_consumed() {
    let (_cancel, runtime, mut state) = child_fixture().await;
    let parent = state.task.parent.unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    let dispatch = crate::meta::dispatch(&mut state, "inbox", json!({})).await;
    assert!(!dispatch.result.is_error);
    assert_eq!(elapsed_ms(&mut state, 50), 10_000);
    runtime
        .send_agent_message(
            parent,
            state.task.run_id,
            AgentMessageKind::Steering,
            "new work",
            None,
        )
        .unwrap();
    assert_eq!(elapsed_ms(&mut state, 100), 10_000);
    let dispatch = crate::meta::dispatch(&mut state, "inbox", json!({})).await;
    assert!(!dispatch.result.is_error);
    assert_eq!(elapsed_ms(&mut state, 150), 0);
}

#[tokio::test(start_paused = true)]
async fn output_bearing_waits_renew_when_they_consume_parent_messages() {
    for tool in ["wait", "wait_reply"] {
        let (_cancel, runtime, mut state) = child_fixture().await;
        let parent = state.task.parent.unwrap();
        let caller = state.task.run_id;
        tokio::time::advance(Duration::from_secs(10)).await;
        let input = if tool == "wait_reply" {
            let sent = runtime
                .send_agent_message(caller, parent, AgentMessageKind::Send, "scope?", None)
                .unwrap();
            // Sending a request is not consuming parent input.
            assert_eq!(elapsed_ms(&mut state, 50), 10_000);
            runtime
                .send_agent_message(
                    parent,
                    caller,
                    AgentMessageKind::Reply,
                    "scope",
                    Some(sent.clone()),
                )
                .unwrap();
            json!({"message_id": sent, "timeout_ms": 60_000})
        } else {
            runtime
                .send_agent_message(parent, caller, AgentMessageKind::Send, "scope", None)
                .unwrap();
            json!({"run_ids": [parent.to_string()]})
        };
        let dispatch = crate::meta::dispatch(&mut state, tool, input).await;
        assert!(!dispatch.result.is_error, "{}", dispatch.result.content);
        assert!(dispatch.result.content.contains("scope"));
        assert_eq!(elapsed_ms(&mut state, 100), 0);
        assert!(runtime.take_inbox(caller).unwrap().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn parent_answers_reset_once_on_injection_not_when_stored() {
    let (_cancel, runtime, mut state) = child_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let question = runtime
        .request_user_question(state.task.run_id, "scope?".into(), vec![], false)
        .unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    runtime
        .answer_subagent_question(state.task.parent.unwrap(), &question.id, "narrow scope")
        .await
        .unwrap();
    assert_eq!(elapsed_ms(&mut state, 50), 10_000);
    assert!(state.flush_user_answers().unwrap());
    assert_eq!(elapsed_ms(&mut state, 100), 0);
    tokio::time::advance(Duration::from_secs(7)).await;
    assert!(!state.flush_user_answers().unwrap());
    assert_eq!(elapsed_ms(&mut state, 150), 7_000);
}
