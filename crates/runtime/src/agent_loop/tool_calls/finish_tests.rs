use super::*;
use crate::{RunInterrupt, RunStore};
use providers::{ContentBlock, Message, Role as MessageRole};
use serde_json::json;

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
