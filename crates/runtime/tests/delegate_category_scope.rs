//! 委譲ツールの worker/reviewer カテゴリとロールの一致境界を検証する。

mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

async fn delegate_case(args: Value, accepted: bool) -> String {
    // Given: 親と子を識別できるスクリプトとカタログ未接続のランタイム
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "ORCH",
            [
                Ok(tool_response("delegate-child", "delegate", args)),
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
        let child = runtime
            .list_agents()
            .into_iter()
            .find(|agent| agent.parent_run_id == Some(parent))
            .expect("delegated child run")
            .run_id;
        assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));
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
    let text = content
        .iter()
        .map(|item| match item {
            ToolResultContent::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.is_empty());
    text
}

#[tokio::test]
async fn delegate_routes_public_targets_in_awaited_and_background_modes() {
    for background in [false, true] {
        for role in [
            "worker",
            "explorer",
            "planner",
            "reviewer",
            "multimodallooker",
        ] {
            delegate_case(
                json!({"target":{"role":role}, "background":background, "prompt":"CHILD"}),
                true,
            )
            .await;
        }
        for category in config::agent_categories::public_categories() {
            delegate_case(json!({"target":{"role":category.role,"category":category.id.as_str()}, "background":background, "prompt":"CHILD"}), true).await;
        }
    }
}

#[tokio::test]
async fn delegate_rejects_invalid_targets_before_spawning_in_both_modes() {
    for background in [false, true] {
        for target in [
            json!({"role":"worker","category":"plan-review"}),
            json!({"role":"reviewer","category":"quick"}),
            json!({"role":"explorer","category":"quick"}),
            json!({"role":"planner","category":"plan-review"}),
            json!({"role":"worker","category":"plan"}),
            json!({"role":"worker","category":"lesson"}),
            json!({"role":"reviewer","category":"lesson_review"}),
            json!({"role":"reviewer","category":"tool-execution"}),
            json!({"role":"worker","category":"conversation"}),
            json!({"role":"worker","category":null}),
            json!({"role":null}),
            json!({"role":"Planner"}),
            json!({"role":"unknown"}),
            json!({"role":"worker","extra":true}),
            json!({"category":"quick"}),
        ] {
            let args = json!({"target":target,"background":background,"prompt":"CHILD"});
            delegate_case(args, false).await;
        }
        for mut args in [
            json!({"prompt":"CHILD"}),
            json!({"role":"worker","category":"quick","prompt":"CHILD"}),
            json!({"target":{"role":"worker"},"role":"planner","prompt":"CHILD"}),
            json!({"target":{"role":"worker"},"category":"quick","prompt":"CHILD"}),
            json!({"target":{"role":"worker"},"extra":true,"prompt":"CHILD"}),
        ] {
            args["background"] = json!(background);
            delegate_case(args, false).await;
        }
    }
}

/// Parses the `Use target={...}.` correction out of an actual error response.
fn correction(error: &str) -> Value {
    let (_, rest) = error.split_once("Use target=").expect("correction");
    let (target, _) = rest.split_once("}.").expect("correction end");
    serde_json::from_str(&format!("{target}}}")).unwrap()
}

#[tokio::test]
async fn invalid_planner_category_returns_an_executable_planner_correction() {
    for (category, owner) in [
        ("plan", None),
        ("plan-review", Some("reviewer")),
        ("deep", Some("worker")),
        ("research", Some("worker")),
    ] {
        let error = delegate_case(
            json!({"target":{"role":"planner","category":category},"prompt":"CHILD"}),
            false,
        )
        .await;
        // The hint names the given category's owner rather than an unrelated example.
        assert_eq!(
            owner.map(|owner| format!("Category `{category}` belongs to target.role={owner}.")),
            error
                .split_once(" Category ")
                .map(|(_, hint)| format!("Category {hint}")),
            "{error}"
        );
        let target = correction(&error);
        assert_eq!(target, json!({"role":"planner"}));
        delegate_case(json!({"target":target,"prompt":"CHILD"}), true).await;
    }
}

#[tokio::test]
async fn misplaced_top_level_target_returns_the_callers_own_target() {
    for (args, expected) in [
        (
            json!({"role":"worker","category":"quick"}),
            json!({"role":"worker","category":"quick"}),
        ),
        (
            json!({"category":"plan-review"}),
            json!({"role":"reviewer","category":"plan-review"}),
        ),
        (json!({"role":"explorer"}), json!({"role":"explorer"})),
    ] {
        let mut call = args.clone();
        call["prompt"] = json!("CHILD");
        let error = delegate_case(call, false).await;
        let target = correction(&error);
        assert_eq!(target, expected, "{error}");
        delegate_case(json!({"target":target,"prompt":"CHILD"}), true).await;
    }
}

#[test]
fn reviewer_categories_keep_standard_reviewer_capabilities() {
    let standard = runtime::ExecutionPolicy::for_role(Role::Reviewer);
    for category in ["plan-review", "tool-execution"] {
        let policy = standard.clone().for_run_config(
            &RunConfig {
                category: Some(category.into()),
                ..Default::default()
            },
            false,
        );
        assert_eq!(policy, standard);
        for tool in [
            "shell",
            "edit",
            "write",
            "submit_lesson_review",
            "inspect_learning_source",
        ] {
            assert!(
                policy.authorize(tool).is_err(),
                "{category} must not grant {tool}"
            );
        }
    }
}
