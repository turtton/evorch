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

async fn assert_wire_cache_after_wait(before: &[Message], after: &[Message]) {
    use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
    use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
    use providers::wire::responses::{ResponsesDialect, ResponsesRequestOptions};
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
    let options = |dialect| ResponsesRequestOptions {
        dialect,
        prompt_cache_key: Some("stable-affinity".into()),
        codex_model: None,
    };
    for dialect in [
        None,
        Some(ResponsesDialect::OpenAi),
        Some(ResponsesDialect::Codex),
    ] {
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache(
            (0..2)
                .map(|_| ScriptedResponse::text_stream("response", "mock-model", ["ok"]))
                .collect(),
        );
        let encode = |messages: &[Message]| match dialect {
            None => serde_json::to_value(providers::wire::openai::to_wire_request(
                &request(messages),
                false,
            ))
            .unwrap(),
            Some(dialect) => {
                let mut value = serde_json::to_value(providers::wire::responses::to_wire_request(
                    &request(messages),
                    &options(dialect),
                ))
                .unwrap();
                value["stream"] = json!(false);
                value
            }
        };
        let client = reqwest::Client::new();
        let path = if dialect.is_none() {
            "chat/completions"
        } else {
            "responses"
        };
        let mut usage = Vec::new();
        for messages in [before, after] {
            let body = client
                .post(format!("{}/{path}", mock.base_url()))
                .json(&encode(messages))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .text()
                .await
                .unwrap();
            let observed = if dialect.is_none() {
                serde_json::from_str::<serde_json::Value>(&body).unwrap()["usage"].clone()
            } else {
                body.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
                    .find(|event| event["type"] == "response.completed")
                    .expect("mock emits a terminal response")["response"]["usage"]
                    .clone()
            };
            usage.push(observed);
        }
        let captured = mock.recorded_requests();
        assert_eq!(captured.len(), 2);
        assert_append_only(
            if dialect.is_none() {
                CacheProtocol::OpenAi
            } else {
                CacheProtocol::Codex
            },
            &captured[0].body,
            &captured[1].body,
        )
        .unwrap();
        let (input_key, detail_key) = if dialect.is_none() {
            ("prompt_tokens", "prompt_tokens_details")
        } else {
            ("input_tokens", "input_tokens_details")
        };
        assert!(usage[0][input_key].as_u64().unwrap() > 0);
        assert_eq!(usage[1][detail_key]["cached_tokens"], usage[0][input_key]);
        let body = &captured[1].body;
        let outputs: Vec<_> = if dialect.is_none() {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item["tool_call_id"].as_str())
                .collect()
        } else {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .filter_map(|item| item["call_id"].as_str())
                .collect()
        };
        assert_eq!(outputs, ["read", "wait-all", "wait-legacy"]);
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
                json!({"run_ids":[child.to_string()],"mode":"all"}),
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
    assert_wire_cache_after_wait(&provider_prefix, &state.context.messages).await;
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
