use super::*;
use crate::{AgentContext, Role, RunConfig};
use providers::{Message, Role as MessageRole};
use serde_json::json;

fn call(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

#[tokio::test]
async fn target_completion_while_parked_does_not_observe_or_rewind_its_generation() {
    let (_dir, _storage, runtime, mut state) =
        crate::agent_loop::continuity_tests::stored_fixture_in("parked-wait.db", false).await;
    let (cancel, cancel_rx) = tokio::sync::watch::channel(crate::RunInterrupt::None);
    state.channels.cancel_rx = cancel_rx;
    let parent = state.task.run_id;
    state.task.publication = runtime.execution_snapshot(parent).unwrap().publication();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![call("wait", "wait", json!({"run_id":child.to_string()}))],
    });
    let before = state.context.messages.clone();
    runtime.request_quiesce();
    let parked = state.quiesce_tools_waiting(None);
    tokio::pin!(parked);
    assert!(futures_util::poll!(&mut parked).is_pending());
    let intent = runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|i| i.run_id == parent)
        .unwrap();
    assert_eq!(
        intent.checkpoint.unwrap().boundary,
        crate::checkpoint::CheckpointBoundary::ToolsWaiting
    );
    assert!(intent.tool_intent.is_empty());
    let bundle = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_bundle(parent)
        .unwrap()
        .unwrap();
    let descriptor: crate::restore::RunRestoreDescriptor =
        serde_json::from_str(&bundle.record.config_json).unwrap();
    crate::restore::validate_recovery_waits(&bundle.record, &descriptor.recovery_waits).unwrap();
    runtime.cancel(child).unwrap();
    let _ = runtime.wait(child).await;
    let notice = runtime
        .run_output(parent, child)
        .unwrap()
        .completion_notice
        .unwrap();
    assert!(!notice.observed());
    let generation = notice.generation;
    assert!(futures_util::poll!(&mut parked).is_pending());
    assert_eq!(
        runtime
            .run_output(parent, child)
            .unwrap()
            .completion_notice
            .unwrap()
            .generation,
        generation
    );
    assert!(!notice.observed());
    assert_eq!(
        serde_json::from_str::<Vec<Message>>(&bundle.record.messages_json).unwrap(),
        before
    );
    cancel.send_replace(crate::RunInterrupt::Stop);
    parked.await;
}

struct UnobservedShell;
#[async_trait::async_trait]
impl tools::Tool for UnobservedShell {
    fn name(&self) -> &str {
        "shell"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::read_only()
    }
    fn has_unobserved_shell_jobs(&self, _: &str) -> bool {
        true
    }
    async fn execute(&self, _: serde_json::Value) -> Result<tools::ToolResult, tools::ToolError> {
        panic!("safety proof must not execute a shell")
    }
}

#[tokio::test]
async fn unobserved_shell_job_prevents_tools_waiting_persistence() {
    let (_dir, _storage, runtime, mut state) =
        crate::agent_loop::continuity_tests::stored_fixture_in("unobserved-wait.db", false).await;
    let child = runtime
        .delegate_background_as_child(
            state.task.run_id,
            Role::Worker,
            "child",
            RunConfig::default(),
        )
        .unwrap();
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![call("wait", "wait", json!({"run_id":child.to_string()}))],
    });
    state.recovery_waits = state.pending_waits(None).unwrap();
    let mut executor = tools::ToolExecutor::new(state.shared.bus.clone());
    executor
        .register(std::sync::Arc::new(UnobservedShell))
        .unwrap();
    state.shared.executor = std::sync::Arc::new(executor);
    assert!(matches!(
        crate::restore::persist_tools_waiting(&state),
        Err(crate::restore::SnapshotError::UnrecordedShellEffects)
    ));
    assert!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(state.task.run_id)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn tools_waiting_cache_prefix_and_batch_order_survive_persist_load_and_resume() {
    let (_dir, storage, runtime, mut state) =
        crate::agent_loop::continuity_tests::stored_fixture_in("tools-waiting.db", false).await;
    let (_cancel, cancel_rx) = tokio::sync::watch::channel(crate::RunInterrupt::None);
    state.channels.cancel_rx = cancel_rx;
    state
        .transition(event_bus::AgentRunPhase::Running, None)
        .unwrap();
    let parent = state.task.run_id;
    state.task.publication = runtime.execution_snapshot(parent).unwrap().publication();
    let provider_prefix = state.context.messages.clone();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![
            call("read", "read", json!({"path":"file"})),
            call(
                "wait-all",
                "wait",
                json!({"run_ids":[child.to_string()],"mode":"all","timeout_ms":1234}),
            ),
            call("wait-legacy", "wait", json!({"run_id":child.to_string()})),
        ],
    });
    state
        .context
        .push_tool_result("read", tools::ToolResult::success("already done"));
    let prefix = state.context.messages.clone();
    assert!(matches!(
        crate::restore::persist_checkpoint(&state).unwrap(),
        crate::checkpoint::SnapshotReference::None {
            reason: crate::checkpoint::SnapshotNoneReason::Skipped
        }
    ));
    state.recovery_waits = state.pending_waits(None).unwrap();
    assert_eq!(state.recovery_waits.len(), 2);
    crate::restore::persist_tools_waiting(&state).unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    let bundle = store.restore_bundle(parent).unwrap().unwrap();
    let descriptor: crate::restore::RunRestoreDescriptor =
        serde_json::from_str(&bundle.record.config_json).unwrap();
    assert!(descriptor.interrupted_tool_calls.is_empty());
    crate::restore::validate_recovery_waits(&bundle.record, &descriptor.recovery_waits).unwrap();
    let restored =
        crate::restore::RestoredState::from_recovery_bundle(&bundle, &descriptor.recovery_waits)
            .unwrap();
    // Independent serialized input-prefix assertion: do not only trust a cache
    // counter or compare continuation structs.
    assert_eq!(
        serde_json::to_value(&restored.messages).unwrap(),
        serde_json::to_value(&prefix).unwrap(),
    );
    state.context = AgentContext::from_restored(
        parent,
        Role::Orchestrator,
        restored.messages,
        restored.checkpoints,
        restored.responses_replay,
        restored.snapshot_revision,
    );
    state.recovery_waits = restored.recovery_waits;
    runtime.cancel(child).unwrap();
    let _ = runtime.wait(child).await;
    let endpoints_before: Vec<_> = runtime.list_agents().iter().map(|run| run.run_id).collect();
    assert!(state.resume_recovery_waits().await);
    assert!(state.recovery_waits.is_empty());
    assert_eq!(&state.context.messages[..prefix.len()], prefix);
    let ids: Vec<_> = state
        .context
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| {
            if let ContentBlock::ToolResult { tool_call_id, .. } = b {
                Some(tool_call_id.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ids, ["read", "wait-all", "wait-legacy"]);
    assert_eq!(
        runtime
            .list_agents()
            .iter()
            .map(|run| run.run_id)
            .collect::<Vec<_>>(),
        endpoints_before
    );
    assert!(state.resume_recovery_waits().await);
    assert_eq!(state.context.messages.len(), prefix.len() + 2);
    let request = |messages: &[Message]| providers::ChatRequest {
        model: "mock-model".into(),
        messages: messages.to_vec(),
        tools: vec![
            providers::ToolSpec {
                name: "wait".into(),
                description: "observe existing runs".into(),
                input_schema: json!({"type":"object"}),
            },
            providers::ToolSpec {
                name: "read".into(),
                description: "read only".into(),
                input_schema: json!({"type":"object"}),
            },
        ],
        temperature: Some(0.2),
        max_tokens: Some(300),
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        tool_choice: None,
        observation: None,
        responses_replay: None,
    };
    crate::recovery_cache_test_support::assert_wait_cache(
        &request(&provider_prefix),
        &request(&state.context.messages),
        &["read", "wait-all", "wait-legacy"],
    )
    .await;
    drop(storage);
}

#[tokio::test]
async fn wait_followed_by_unstarted_external_call_is_fail_closed() {
    let (_dir, _storage, runtime, mut state) =
        crate::agent_loop::continuity_tests::stored_fixture_in("unsafe-wait.db", false).await;
    let child = runtime
        .delegate_background_as_child(
            state.task.run_id,
            Role::Worker,
            "child",
            RunConfig::default(),
        )
        .unwrap();
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![
            call("wait", "wait", json!({"run_id":child.to_string()})),
            call("external", "shell", json!({"command":"touch external"})),
        ],
    });
    assert!(state.pending_waits(None).is_err());
    assert!(crate::restore::persist_tools_waiting(&state).is_err());
    assert!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(state.task.run_id)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delegate_projection_uses_original_call_order_and_existing_child_ids() {
    let (_dir, _storage, runtime, mut state) =
        crate::agent_loop::continuity_tests::stored_fixture_in("delegate-wait.db", false).await;
    let (_cancel, cancel_rx) = tokio::sync::watch::channel(crate::RunInterrupt::None);
    state.channels.cancel_rx = cancel_rx;
    state
        .transition(event_bus::AgentRunPhase::Running, None)
        .unwrap();
    let parent = state.task.run_id;
    let mut children = Vec::new();
    for id in ["call-a", "call-b"] {
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, id, RunConfig::default())
            .unwrap();
        children.push(crate::quiesce::DelegateWaitCall {
            tool_call_id: id.into(),
            child,
        });
    }
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: children
            .iter()
            .map(|c| {
                call(
                    &c.tool_call_id,
                    "delegate",
                    json!({"target":{"role":"worker"},"prompt":c.tool_call_id}),
                )
            })
            .collect(),
    });
    state.delegate_wait_calls = children.clone();
    state.recovery_waits = state.pending_waits(None).unwrap();
    assert_eq!(state.recovery_waits.len(), 2);
    let mut mismatched = state.recovery_waits.clone();
    if let WaitKind::Delegates { calls } =
        &mut mismatched[0].continuation.payload.as_mut().unwrap().kind
    {
        calls.swap(0, 1);
    }
    assert!(
        crate::restore::waiting::validate_wait_messages(
            parent,
            &state.context.messages,
            &mismatched,
        )
        .is_err()
    );
    let count = runtime.list_agents().len();
    for child in &children {
        runtime.cancel(child.child).unwrap();
        let _ = runtime.wait(child.child).await;
    }
    assert!(state.resume_recovery_waits().await);
    let ids: Vec<_> = state
        .context
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| {
            if let ContentBlock::ToolResult { tool_call_id, .. } = b {
                Some(tool_call_id.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ids, ["call-a", "call-b"]);
    assert_eq!(runtime.list_agents().len(), count);
}

#[tokio::test]
async fn reply_continuation_consumes_only_the_correlated_reply_once() {
    reply_continuation(false).await;
}

#[tokio::test]
async fn expired_recovery_reply_times_out_once_without_consuming_queued_reply() {
    reply_continuation(true).await;
}

async fn reply_continuation(expired: bool) {
    use event_bus::AgentMessageKind;
    let (_dir, _storage, runtime, mut state) =
        crate::agent_loop::continuity_tests::stored_fixture_in("reply-continuation.db", false)
            .await;
    let (_cancel, cancel_rx) = tokio::sync::watch::channel(crate::RunInterrupt::None);
    state.channels.cancel_rx = cancel_rx;
    state
        .transition(event_bus::AgentRunPhase::Running, None)
        .unwrap();
    let parent = state.task.run_id;
    state.task.publication = runtime.execution_snapshot(parent).unwrap().publication();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    let message = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "request", None)
        .unwrap();
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![call(
            "reply",
            "wait_reply",
            json!({"message_id":message,"timeout_ms":1000}),
        )],
    });
    // An elapsed recovery deadline wins; live waits retain their ordinary priority.
    state.recovery_waits = state
        .pending_waits(Some(("reply", if expired { 1 } else { u64::MAX })))
        .unwrap();
    let saved_waits = state.recovery_waits.clone();
    crate::restore::persist_tools_waiting(&state).unwrap();
    let bundle = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_bundle(parent)
        .unwrap()
        .unwrap();
    let restored =
        crate::restore::RestoredState::from_recovery_bundle(&bundle, &saved_waits).unwrap();
    assert_eq!(restored.recovery_waits, saved_waits);
    state.recovery_waits = restored.recovery_waits;
    let unrelated = runtime
        .send_agent_message(child, parent, AgentMessageKind::Send, "keep queued", None)
        .unwrap();
    let reply = runtime
        .send_agent_message(
            child,
            parent,
            AgentMessageKind::Reply,
            "reply body",
            Some(message.clone()),
        )
        .unwrap();
    let prefix = state.context.messages.clone();
    assert!(state.resume_recovery_waits().await);
    assert_eq!(&state.context.messages[..prefix.len()], prefix);
    let ContentBlock::ToolResult {
        tool_call_id,
        content,
        is_error,
    } = &state.context.messages.last().unwrap().content[0]
    else {
        panic!("reply result")
    };
    assert_eq!(tool_call_id, "reply");
    assert_eq!(*is_error, expired);
    let providers::ToolResultContent::Text { text } = &content[0] else {
        panic!("reply text")
    };
    if !expired {
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["message_id"], reply);
        assert_eq!(value["content"], "reply body");
    }
    assert!(runtime.check_reply_wait(parent, &message).is_err());
    assert_eq!(
        runtime
            .take_inbox(parent)
            .unwrap()
            .iter()
            .map(|m| &m.message_id)
            .collect::<Vec<_>>(),
        if expired {
            vec![&unrelated, &reply]
        } else {
            vec![&unrelated]
        }
    );
    assert!(state.resume_recovery_waits().await);
    assert_eq!(state.context.messages.len(), prefix.len() + 1);

    let expired = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "new request", None)
        .unwrap();
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![call(
            "expired",
            "wait_reply",
            json!({"message_id":expired,"timeout_ms":1000}),
        )],
    });
    state.recovery_waits = state.pending_waits(Some(("expired", 1))).unwrap();
    let before_timeout = state.context.messages.len();
    assert!(state.resume_recovery_waits().await);
    assert!(matches!(&state.context.messages.last().unwrap().content[0],
        ContentBlock::ToolResult {tool_call_id,is_error:true,..} if tool_call_id=="expired"));
    assert!(state.resume_recovery_waits().await);
    assert_eq!(state.context.messages.len(), before_timeout + 1);
}
