use super::*;
use crate::{RunInterrupt, RunStore};
use providers::{ContentBlock, Message, Role as MessageRole};
use serde_json::json;
use tools::ToolExecutor;

#[tokio::test]
async fn answer_after_finish_dispatch_closes_tail_before_next_provider_input() {
    let (_cancel, cancel_rx) = tokio::sync::watch::channel(RunInterrupt::None);
    let (runtime, mut state) = super::super::delegate_cleanup_tests::fixture(cancel_rx);
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    state
        .transition(event_bus::AgentRunPhase::Running, None)
        .unwrap();
    state.context.push_user("preserve this already sent prefix");
    let prefix = state.context.messages.clone();
    let question = runtime
        .request_user_question(
            state.caller_run_id(),
            "Which implementation?".into(),
            vec![],
            false,
        )
        .unwrap();
    let batch = vec![
        (
            "finish".into(),
            "finish".into(),
            json!({"result":"Independent work completed"}),
        ),
        ("tail".into(), "read".into(), json!({"path":"unused"})),
    ];
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: batch
            .iter()
            .map(
                |(id, name, input): &(String, String, Value)| ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
            )
            .collect(),
    });
    // Fix the concurrent ordering: dispatch accepts finish, then an answer arrives
    // before the completion gate rechecks readiness and closes the mailbox.
    let dispatch = meta::dispatch(&mut state, "finish", batch[0].2.clone()).await;
    let meta::Terminal::Finish(result) = dispatch.terminal else {
        panic!("finish must pass its initial readiness check")
    };
    state.context.push_tool_result("finish", dispatch.result);
    runtime.answer_user_question(&question.id, "Use A").unwrap();
    let mut events = state.shared.bus.subscribe();
    assert!(
        state
            .finish_tool_batch(&result, batch.into_iter().skip(1))
            .await
    );
    assert!(state.flush_user_answers().unwrap());

    // This is the next provider input: every pending tool call must be resolved
    // before final text or the new answer, without running the terminal batch tail.
    assert_eq!(&state.context.messages[..prefix.len()], prefix);
    let mut pending = std::collections::HashSet::new();
    let mut results = Vec::new();
    for message in &state.context.messages[prefix.len()..] {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    assert!(pending.insert(id));
                }
                ContentBlock::ToolResult {
                    tool_call_id,
                    is_error,
                    ..
                } => {
                    assert!(pending.remove(tool_call_id));
                    results.push((tool_call_id.as_str(), *is_error));
                }
                ContentBlock::Text { .. } => assert!(
                    pending.is_empty(),
                    "unresolved tool calls must not cross a text or answer message"
                ),
                _ => {}
            }
        }
    }
    assert!(pending.is_empty());
    assert_eq!(results, [("finish", false), ("tail", true)]);
    assert!(!events.drain_pending_snapshot().iter().any(|event| matches!(&event.kind, event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted { call_id, .. } | event_bus::ToolEvent::ToolCompleted { call_id, .. } | event_bus::ToolEvent::ExecutionDenied { call_id, .. } | event_bus::ToolEvent::ApprovalRequested { call_id, .. }) if call_id == "tail")));
    assert!(state.pending_terminal.is_none());
    runtime.cancel(state.caller_run_id()).unwrap();
    runtime.wait(state.caller_run_id()).await.unwrap();
}

/// A terminal shell result and its host-input wake share the batch boundary.
/// The process supervisor's terminal/input priority is tested in tools; this
/// seam controls exactly when that input is accepted relative to receiver drop.
#[derive(Debug)]
struct CompletedShellHandoff {
    inbox: Arc<crate::runtime::user_inbox::UserInbox>,
    sender: tokio::sync::mpsc::Sender<crate::runtime::user_inbox::UserInput>,
    after_receiver: bool,
}
#[async_trait::async_trait]
impl tools::Tool for CompletedShellHandoff {
    fn name(&self) -> &'static str {
        "shell"
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::process()
    }
    async fn execute(&self, _: Value) -> Result<ToolResult, tools::ToolError> {
        unreachable!()
    }
    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        _: Value,
    ) -> Result<ToolResult, tools::ToolError> {
        let receiver = ctx.shell_input.as_ref().unwrap().begin_wait();
        let mut receiver = Some(receiver);
        if self.after_receiver {
            drop(receiver.take());
        }
        self.inbox
            .enqueue(|seq| {
                self.sender.try_send((
                    "reserved followup".into(),
                    vec![],
                    true,
                    Some("followup".into()),
                    seq,
                ))
            })
            .unwrap();
        drop(receiver);
        Ok(ToolResult::success("exit_code: 0\nshell completed\n"))
    }
}

#[tokio::test]
async fn shell_then_finish_preserves_simultaneous_and_post_receiver_input_once() {
    for after_receiver in [false, true] {
        let (_cancel, cancelled) = tokio::sync::watch::channel(RunInterrupt::None);
        let (runtime, mut state) =
            super::super::delegate_cleanup_tests::started_fixture(cancelled).await;
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("handoff.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let run = state.caller_run_id();
        storage
            .handle()
            .append_host_input_accepted(
                &[],
                &[],
                None,
                &storage::HostInputReceipt {
                    prompt_id: "followup".into(),
                    run_id: run.to_string(),
                    root_run_id: run.to_string(),
                    text: "reserved followup".into(),
                    image_count: 0,
                    status: storage::HostInputStatus::Accepted,
                    reason: None,
                    accepted_at_ns: 1,
                    status_at_ns: 1,
                },
            )
            .unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        state.channels.inbox_rx = receiver;
        let mut executor = ToolExecutor::new(state.shared.bus.clone());
        executor
            .register(Arc::new(CompletedShellHandoff {
                inbox: state.channels.user_inbox.clone(),
                sender,
                after_receiver,
            }))
            .unwrap();
        state.shared.executor = Arc::new(executor);
        state.task.role = crate::Role::Worker;
        state.policy = crate::ExecutionPolicy::for_role(crate::Role::Worker);
        // Exercise the terminal batch seam with both operations admitted; the
        // default Worker role alone does not expose the finish meta tool.
        state
            .policy
            .capabilities
            .allowed_tools
            .insert("finish".into());
        state
            .transition(event_bus::AgentRunPhase::Running, None)
            .unwrap();
        let calls = vec![
            ("shell".into(), "shell".into(), json!({})),
            ("finish".into(), "finish".into(), json!({"result":"done"})),
        ];
        state.context.push_assistant(Message {
            role: MessageRole::Assistant,
            content: calls
                .iter()
                .map(
                    |(id, name, input): &(String, String, Value)| ContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    },
                )
                .collect(),
        });
        assert!(
            state
                .execute_tools(
                    calls,
                    &crate::AgentInvocationContext::default(),
                    state.shared.model.clone().as_ref()
                )
                .await
        );
        assert!(
            state.pending_terminal.is_none(),
            "finish must leave the next request alive"
        );
        assert_eq!(state.channels.user_inbox.status().pending, 0);
        assert!(!state.flush_user_messages());
        let next_request = serde_json::to_string(&state.context.messages).unwrap();
        assert_eq!(next_request.matches("reserved followup").count(), 1);
        crate::restore::persist_checkpoint(&state).unwrap();
        let receipts = runtime.host_input_receipts(&["followup".into()]).unwrap();
        assert_eq!(receipts[0].status, storage::HostInputStatus::Delivered);
        let record = storage::Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&record.host_inputs_json).unwrap(),
            ["followup"]
        );
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
        assert_eq!(
            runtime.host_input_receipts(&["followup".into()]).unwrap()[0].status,
            storage::HostInputStatus::Delivered
        );
    }
}
