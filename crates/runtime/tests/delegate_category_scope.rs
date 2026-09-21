//! 委譲ツールの worker 専用カテゴリ境界を検証する。

mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig, RunId};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

async fn delegate_case(tool: &str, args: Value, accepted: bool) {
    // Given: 親と子を識別できるスクリプトとカタログ未接続のランタイム
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "ORCH",
            [
                Ok(tool_response("delegate-child", tool, args)),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [Ok(text_response("child done", FinishReason::Stop))],
        )
        .await;
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model.clone());

    // When: 実際のモデル応答から委譲ツールを dispatch する
    let parent =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_owned(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    if accepted {
        assert_eq!(
            runtime.wait(RunId::new(parent.get() + 1)).await,
            Ok(AgentRunPhase::Done)
        );
    }

    // Then: 拒否ならエラー結果のみ、受理なら子の登録とモデル呼び出しがある
    let observed = model.observed().await;
    let (content, is_error) = observed
        .iter()
        .flatten()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == "delegate-child" => Some((content, *is_error)),
            _ => None,
        })
        .expect("親の次ターンに委譲結果がある");
    assert_eq!(runtime.list_agents().len(), if accepted { 2 } else { 1 });
    let child_seen = observed.iter().flatten().any(|message| {
        message.role == providers::Role::User
            && message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == "CHILD"))
    });
    assert_eq!(child_seen, accepted);
    assert_eq!(is_error, !accepted);
    if !accepted {
        assert!(content.iter().any(|item| matches!(
            item,
            ToolResultContent::Text { text } if text.contains("worker")
        )));
    }
}

#[tokio::test]
async fn delegate_accepts_category_when_role_is_omitted() {
    delegate_case(
        "delegate",
        json!({"category": "quick", "prompt": "CHILD"}),
        true,
    )
    .await;
}

#[tokio::test]
async fn delegate_rejects_category_when_role_is_explorer() {
    delegate_case(
        "delegate",
        json!({"role": "explorer", "category": "quick", "prompt": "CHILD"}),
        false,
    )
    .await;
}

#[tokio::test]
async fn async_delegate_rejects_category_when_role_is_explorer() {
    delegate_case(
        "delegate",
        json!({"background": true, "role": "explorer", "category": "quick", "prompt": "CHILD"}),
        false,
    )
    .await;
}

#[tokio::test]
async fn delegate_accepts_category_when_role_is_worker() {
    delegate_case(
        "delegate",
        json!({"role": "worker", "category": "quick", "prompt": "CHILD"}),
        true,
    )
    .await;
}

#[tokio::test]
async fn async_delegate_accepts_category_when_role_is_worker() {
    delegate_case(
        "delegate",
        json!({"background": true, "role": "worker", "category": "quick", "prompt": "CHILD"}),
        true,
    )
    .await;
}

#[tokio::test]
async fn delegate_accepts_explorer_when_category_is_absent() {
    delegate_case(
        "delegate",
        json!({"role": "explorer", "prompt": "CHILD"}),
        true,
    )
    .await;
}

#[tokio::test]
async fn async_delegate_accepts_explorer_when_category_is_absent() {
    delegate_case(
        "delegate",
        json!({"background": true, "role": "explorer", "prompt": "CHILD"}),
        true,
    )
    .await;
}
