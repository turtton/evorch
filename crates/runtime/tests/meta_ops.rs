mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

fn runtime_with(model: Arc<ScriptedModel>) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, model)
}

fn tool_result(messages: &[providers::Message], call_id: &str) -> Option<(String, bool)> {
    messages.iter().find_map(|message| {
        message.content.iter().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == call_id => content.first().map(|item| match item {
                ToolResultContent::Text { text } => (text.clone(), *is_error),
            }),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
    })
}

#[tokio::test]
async fn orchestrator_dispatches_remaining_runtime_meta_operations() {
    // Given: foreground delegate、一覧・検査、cancel、compact、finish を順に要求する Orchestrator
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "META",
            [
                Ok(tool_response(
                    "delegate",
                    "delegate",
                    json!({ "role": "worker", "prompt": "SYNC" }),
                )),
                Ok(tool_response(
                    "spawn-interactive",
                    "delegate_background",
                    json!({
                        "role": "explorer",
                        "prompt": "HOLD",
                        "interactive": true,
                        "name": "held-explorer"
                    }),
                )),
                Ok(tool_response("list", "list_agents", json!({}))),
                Ok(tool_response(
                    "inspect",
                    "inspect_agent",
                    json!({ "run_id": "run-3" }),
                )),
                Ok(tool_response(
                    "cancel",
                    "cancel",
                    json!({ "run_id": "run-3" }),
                )),
                Ok(tool_response("compact", "compact", json!({}))),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "meta done" }),
                )),
            ],
        )
        .await;
    model
        .add_keyed("SYNC", [Ok(text_response("done", FinishReason::Stop))])
        .await;
    model
        .add_keyed("HOLD", [Ok(text_response("waiting", FinishReason::Stop))])
        .await;
    let runtime = runtime_with(Arc::clone(&model));

    // When: Orchestrator を finish まで実行する
    let orchestrator =
        runtime.delegate_background(Role::Orchestrator, "META".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(orchestrator).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        runtime.wait(runtime.list_agents()[2].run_id).await,
        Ok(AgentRunPhase::Error)
    );

    // Then: 各 ToolResult が実 API の成功値または契約どおりの stub error を返す
    let observed = model.observed().await;
    let final_turn = observed
        .iter()
        .rfind(|messages| {
            messages.first().is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "META"))
            })
        })
        .expect("orchestrator final model turn");
    assert_eq!(
        tool_result(final_turn, "delegate"),
        Some(("Done".to_string(), false))
    );
    assert_eq!(
        tool_result(final_turn, "spawn-interactive"),
        Some(("run-3".to_string(), false))
    );
    let (list, list_error) = tool_result(final_turn, "list").expect("list result");
    assert!(!list_error);
    let summaries: serde_json::Value = serde_json::from_str(&list).expect("list JSON");
    assert_eq!(summaries.as_array().expect("summary array").len(), 3);
    // Then: 一覧の identity 項目は meta-op 経由の委譲でも反映される
    assert_eq!(summaries[2]["name"], json!("held-explorer"));
    assert_eq!(summaries[2]["role_name"], json!("Explorer"));
    assert!(
        summaries[2]["model"]
            .as_str()
            .is_some_and(|model| !model.is_empty())
    );
    assert_eq!(summaries[0]["name"], json!("Orchestrator"));
    let (inspection, inspect_error) = tool_result(final_turn, "inspect").expect("inspect result");
    assert!(!inspect_error);
    let inspection: serde_json::Value = serde_json::from_str(&inspection).expect("inspection JSON");
    assert_eq!(inspection["run_id"], json!(3));
    assert_eq!(
        tool_result(final_turn, "cancel"),
        Some(("cancelled".to_string(), false))
    );
    assert_eq!(
        tool_result(final_turn, "compact"),
        Some(("context-engine (v0.2) で提供予定".to_string(), true))
    );
}

#[tokio::test]
async fn invalid_meta_arguments_return_error_and_run_continues() {
    // Given: 不正引数・未知 run・未知 role の後に finish を要求する Orchestrator
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("bad-wait", "wait", json!({ "run_id": 9 }))),
        Ok(tool_response(
            "unknown-message",
            "send_message",
            json!({ "run_id": "run-999", "message": "hello" }),
        )),
        Ok(tool_response(
            "unknown-inspect",
            "inspect_agent",
            json!({ "run_id": "run-999" }),
        )),
        Ok(tool_response(
            "unknown-cancel",
            "cancel",
            json!({ "run_id": "run-999" }),
        )),
        Ok(tool_response(
            "unknown-role",
            "delegate_background",
            json!({ "role": "unknown", "prompt": "unused" }),
        )),
        Ok(tool_response(
            "finish",
            "finish",
            json!({ "result": "recovered" }),
        )),
    ]));
    let runtime = runtime_with(Arc::clone(&model));

    // When: run を終端まで実行する
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "INVALID".to_string(),
        RunConfig::default(),
    );

    // Then: 不正引数は ToolResult error になり、次の turn の finish で Done になる
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let observed = model.observed().await;
    let second_turn = observed.get(1).expect("second model turn");
    assert!(matches!(
        tool_result(second_turn, "bad-wait"),
        Some((_, true))
    ));
    let final_turn = observed.last().expect("final model turn");
    for call_id in [
        "unknown-message",
        "unknown-inspect",
        "unknown-cancel",
        "unknown-role",
    ] {
        assert!(matches!(tool_result(final_turn, call_id), Some((_, true))));
    }
    assert_eq!(runtime.list_agents().len(), 1);
}
