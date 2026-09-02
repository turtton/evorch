mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::workspace::{Project, WorktreeManager};
use runtime::{AgentRuntime, IsolatedMounts, RunConfig, WorkspaceMode};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::time::{Duration, sleep, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, init_git_repo, recording_factory, text_response, tool_response};

fn runtime_with(model: Arc<ScriptedModel>) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, model)
}

fn runtime_with_workspace(
    repo: &Path,
    model: Arc<ScriptedModel>,
) -> (AgentRuntime, Arc<Mutex<Vec<IsolatedMounts>>>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let manager =
        WorktreeManager::new(Project::new(repo.to_path_buf()).expect("git リポジトリを検証できる"));
    let (factory, mounts) = recording_factory();
    (
        AgentRuntime::with_workspace_context(bus, executor, model, manager, factory),
        mounts,
    )
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
    assert_eq!(inspection["workspace"]["mode"], json!("shared"));
    assert_eq!(inspection["workspace"]["merge_mode"], json!("branch"));
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
async fn delegate_background_accepts_isolated_workspace_mode() {
    // Given: isolated workspace を要求する delegate_background を返す Orchestrator
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "META",
            [
                Ok(tool_response(
                    "delegate-isolated",
                    "delegate_background",
                    json!({
                        "role": "worker",
                        "prompt": "ISOLATED",
                        "interactive": true,
                        "workspace_mode": "isolated"
                    }),
                )),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "done" }),
                )),
            ],
        )
        .await;
    let (runtime, mounts) = runtime_with_workspace(&repo, Arc::clone(&model));

    // When: meta op を経由して child を生成する
    let parent =
        runtime.delegate_background(Role::Orchestrator, "META".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    let child = runtime.list_agents()[1].run_id;
    timeout(Duration::from_secs(5), async {
        while mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("isolated child workspace setup が期限内に完了する");

    // Then: child inspection が isolated mode を報告する
    assert_eq!(
        runtime
            .inspect_agent(child)
            .expect("child を inspection できる")
            .workspace
            .expect("workspace inspection が常にある")
            .mode,
        WorkspaceMode::Isolated
    );
    runtime
        .cancel(child)
        .expect("interactive child を cancel できる");
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Error));
}

#[tokio::test]
async fn delegate_background_rejects_unknown_workspace_mode() {
    // Given: 未知の workspace_mode を返す Orchestrator
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "invalid-workspace",
            "delegate_background",
            json!({ "role": "worker", "prompt": "unused", "workspace_mode": "hybrid" }),
        )),
        Ok(tool_response(
            "finish",
            "finish",
            json!({ "result": "done" }),
        )),
    ]));
    let runtime = runtime_with(Arc::clone(&model));

    // When: meta op を実行する
    let parent =
        runtime.delegate_background(Role::Orchestrator, "META".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));

    // Then: ToolResult error で child を spawn しない
    let observed = model.observed().await;
    let final_turn = observed.last().expect("orchestrator final model turn");
    assert!(matches!(
        tool_result(final_turn, "invalid-workspace"),
        Some((_, true))
    ));
    assert_eq!(runtime.list_agents().len(), 1);
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
