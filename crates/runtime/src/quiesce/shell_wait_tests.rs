//! Protocol-independent complement to the three-wire shell wait scenarios:
//! a presented shell outcome cannot close only part of a parallel tool batch.
use super::*;
use crate::checkpoint::CheckpointBoundary;
use crate::quiesce::RestartBlockerKind;
use providers::{Message, Role as MessageRole};
use serde_json::json;
use tools::Tool;

#[tokio::test]
async fn partially_presented_shell_batch_never_inserts_notifications_or_safety_acks() {
    let (_dir, _storage, runtime, mut state) =
        super::continuity_tests::stored_fixture_in("shell-batch.db", false).await;
    let run = state.task.run_id;
    state.task.publication = runtime.execution_snapshot(run).unwrap().publication();
    let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    state.channels.cancel_rx = cancel_rx;
    let shell = Arc::new(tools::Shell::new(Arc::new(
        sandbox::DirectSandbox::new_unchecked(),
    )));
    let mut executor = tools::ToolExecutor::new(state.shared.bus.clone());
    executor.register(shell.clone()).unwrap();
    state.shared.executor = Arc::new(executor);
    let tool_context = tools::ToolExecutionContext {
        run_id: run.to_string(),
        call_id: Some("shell-start".into()),
        ..Default::default()
    };
    let handle = shell
        .execute_with_context(
            &tool_context,
            json!({"command":"printf batch-output", "yield_ms":0}),
        )
        .await
        .unwrap();
    let mut changes = shell.subscribe_shell_job_changes().unwrap();
    let outcome = loop {
        let job = shell.shell_job_snapshots(&run.to_string()).remove(0);
        if job.state == tools::ShellJobState::Terminal && job.teardown_confirmed {
            break shell.shell_job_result(&job.identity).unwrap();
        }
        changes.changed().await.unwrap();
    };
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "shell-start".into(),
            name: "shell".into(),
            input: json!({"command":"printf batch-output", "yield_ms":0}),
        }],
    });
    state
        .context
        .push_tool_result("shell-start", tools::ToolResult::success(handle.content));
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: ["shell-wait", "other-wait"]
            .into_iter()
            .map(|id| ContentBlock::ToolUse {
                id: id.into(),
                name: "wait".into(),
                input: json!({}),
            })
            .collect(),
    });
    runtime.request_quiesce();
    let original = state.context.messages.clone();
    assert!(
        state
            .quiesce_safe_point(CheckpointBoundary::ToolsWaiting)
            .await
    );
    assert_eq!(
        state.context.messages, original,
        "an open batch must not gain a user-role completion body"
    );
    let incarnation = state.task.publication.incarnation();
    assert!(!runtime.shared.quiesce.acknowledged(run, &incarnation));
    assert!(
        runtime
            .restart_blockers()
            .iter()
            .any(|b| b.run == run && b.kind == RestartBlockerKind::UnfinishedToolBatch)
    );
    assert!(!shell.shell_job_snapshots(&run.to_string())[0].observed);

    state.context.push_tool_result(
        "shell-wait",
        tools::ToolResult::success(outcome.result.content),
    );
    crate::meta::Observations {
        shell: vec![outcome.snapshot.identity],
        ..Default::default()
    }
    .presented(&mut state);
    assert!(shell.shell_job_snapshots(&run.to_string())[0].observed);
    let partial = state.context.messages.clone();
    assert!(
        state
            .quiesce_safe_point(CheckpointBoundary::ToolsWaiting)
            .await
    );
    assert_eq!(
        state.context.messages, partial,
        "the remaining wait keeps the entire original batch open"
    );
    assert!(!runtime.shared.quiesce.acknowledged(run, &incarnation));
    assert!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .is_none(),
        "no partial ToolsWaiting snapshot may be saved"
    );

    state
        .context
        .push_tool_result("other-wait", tools::ToolResult::success("{}"));
    let complete = state.context.messages.clone();
    let parked = state.quiesce_safe_point(CheckpointBoundary::ToolsApplied);
    tokio::pin!(parked);
    assert!(futures_util::poll!(&mut parked).is_pending());
    assert!(runtime.shared.quiesce.acknowledged(run, &incarnation));
    let intent = runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|i| i.run_id == run)
        .unwrap();
    assert_eq!(
        intent.checkpoint.unwrap().boundary,
        CheckpointBoundary::ToolsApplied
    );
    let saved = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<Message>>(&saved.messages_json).unwrap(),
        complete
    );
    let all_blocks: Vec<_> = complete.iter().flat_map(|m| &m.content).collect();
    for id in ["shell-start", "shell-wait", "other-wait"] {
        assert_eq!(all_blocks.iter().filter(|b| matches!(b, ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == id)).count(), 1);
    }
    cancel.send_replace(RunInterrupt::Stop);
    assert!(!parked.await);
    runtime.cancel(run).unwrap();
    let _ = runtime.wait(run).await;
}
