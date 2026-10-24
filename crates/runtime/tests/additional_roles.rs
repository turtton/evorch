mod support;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use std::sync::Arc;
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

#[test]
fn production_catalog_and_triggers_cover_additional_roles() {
    let config = config::Config::default();
    let catalog = runtime::prompt::build_catalog(&runtime::prompt::CatalogBuildInput {
        config: &config,
        user_presets_dir: None,
        available_agents: &[],
        available_skills: &[],
    })
    .expect("production catalog");
    let triggers = runtime::prompt::default_role_triggers();

    // Given: the production catalog and its role trigger sources.
    // When: additional roles resolve their generic prompts.
    let researcher_prompt = catalog
        .system_prompt_for(Role::WebResearcher, None, "generic")
        .expect("WebResearcher prompt");
    assert!(researcher_prompt.contains("# WebResearcher"));
    assert!(researcher_prompt.contains("公式資料や一次情報"));
    assert!(researcher_prompt.contains("出典 URL"));
    let planner_prompt = catalog
        .system_prompt_for(Role::Planner, None, "generic")
        .expect("Planner prompt");
    let advisor_prompt = catalog
        .system_prompt_for(Role::Advisor, None, "generic")
        .expect("Advisor prompt");
    let looker_prompt = catalog
        .system_prompt_for(Role::MultimodalLooker, None, "generic")
        .expect("MultimodalLooker prompt");
    assert_ne!(planner_prompt, advisor_prompt);
    assert_ne!(planner_prompt, looker_prompt);
    assert_ne!(advisor_prompt, looker_prompt);
    let evaluator_prompt = catalog
        .system_prompt_for(Role::Evaluator, None, "generic")
        .expect("Evaluator prompt");
    assert!(evaluator_prompt.contains("# Evaluator"));

    // Then: each role has a purpose trigger without a duplicated tool roster.
    for role in [
        Role::WebResearcher,
        Role::Planner,
        Role::Advisor,
        Role::MultimodalLooker,
        Role::Evaluator,
    ] {
        let trigger = triggers
            .iter()
            .find(|trigger| trigger.name == role.name())
            .expect("role trigger");
        assert!(!trigger.description.is_empty());
        assert!(!trigger.description.contains("許可ツール:"));
    }
}

#[tokio::test]
async fn delegate_spawns_additional_roles_and_rejects_unknown_names() {
    // Given: a coordinator requesting all new roles and an invalid one.
    let model = Arc::new(ScriptedModel::new([]));
    model.add_keyed("ROOT", [
        Ok(tool_response("researcher", "delegate", json!({"target": {"role": "web_researcher"}, "prompt":"CHILD"}))),
        Ok(tool_response("planner", "delegate", json!({"target": {"role": "planner"}, "prompt":"CHILD"}))),
        Ok(tool_response("advisor", "delegate", json!({"target": {"role": "advisor"}, "prompt":"CHILD"}))),
        Ok(tool_response("legacy", "delegate", json!({"target": {"role": "oracle"}, "prompt":"CHILD"}))),
        Ok(tool_response("looker", "delegate", json!({"target": {"role": "multimodal_looker"}, "prompt":"CHILD", "images":[{"media_type":"image/png", "data":"aGVsbG8="}]}))),
        Ok(tool_response("invalid", "delegate", json!({"target": {"role": "invalid"}, "prompt":"CHILD"}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]).await;
    model
        .add_keyed(
            "CHILD",
            (0..5).map(|_| Ok(text_response("done", FinishReason::Stop))),
        )
        .await;
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model.clone());
    // When: the real delegate dispatcher runs.
    let root = runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    assert_eq!(runtime.wait(root).await, Ok(AgentRunPhase::Done));
    // Then: only known roles spawn, and the invalid role returns structured data.
    let runs = runtime.list_agents();
    assert_eq!(runs.len(), 6);
    assert_eq!(
        runs.iter().filter(|run| run.role_name == "Advisor").count(),
        2
    );
    for name in ["WebResearcher", "Planner", "Advisor", "MultimodalLooker"] {
        assert!(runs.iter().any(|run| run.role_name == name));
    }
    let observed = model.observed().await;
    assert!(observed.iter().flatten().flat_map(|message| &message.content).any(|block| matches!(block,
        ContentBlock::Image { media_type, data } if media_type == "image/png" && data == "aGVsbG8="
    )));
    let error = observed
        .iter()
        .flatten()
        .flat_map(|message| &message.content)
        .find_map(|block| {
            if let ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error: true,
            } = block
                && tool_call_id == "invalid"
            {
                let ToolResultContent::Text { text } = &content[0] else {
                    panic!("expected text result");
                };
                return Some(
                    serde_json::from_str::<serde_json::Value>(text).expect("structured error"),
                );
            }
            None
        })
        .expect("unknown role result");
    assert_eq!(error["code"], "unknown_role");
    assert_eq!(error["role"], "invalid");
}

#[test]
fn advisor_consultation_and_reviewer_acceptance_have_distinct_contracts() {
    let advisor = config::PresetStore::resolve("role-advisor", None).unwrap();
    let reviewer = config::PresetStore::resolve("role-reviewer", None).unwrap();
    let triggers = runtime::prompt::default_role_triggers();
    let guidance = &triggers
        .iter()
        .find(|t| t.name == "Advisor")
        .unwrap()
        .description;
    for contract in [
        "選択肢",
        "トレードオフ",
        "推奨案",
        "不確実性",
        "判断を変える追加情報",
    ] {
        assert!(advisor.contains(contract), "{contract}");
    }
    for boundary in [
        "Worker",
        "Reviewer",
        "Planner",
        "合否",
        "詳細実行計画",
        "固定工程",
    ] {
        assert!(advisor.contains(boundary), "{boundary}");
        assert!(guidance.contains(boundary), "{boundary}");
    }
    assert!(advisor.contains("別 context"));
    for contract in [
        "実装結果",
        "計画",
        "仕様",
        "合否",
        "Advisor",
        "別 context",
        "承認 / 修正要求",
    ] {
        assert!(reviewer.contains(contract), "{contract}");
    }
    assert_eq!(triggers.len(), 9);
    assert!(!triggers.iter().any(|t| t.name == "Oracle"));
}

/// ADR 0040: 起用条件は Orchestrator には keyTriggers の 1 か所だけで届き、
/// Advisor には自分が呼ばれる場面として届く。
#[test]
fn advisor_stuck_points_reach_the_orchestrator_once_and_the_advisor() {
    let config = config::Config::default();
    let catalog = runtime::prompt::build_catalog(&runtime::prompt::CatalogBuildInput {
        config: &config,
        user_presets_dir: None,
        available_agents: &[],
        available_skills: &[],
    })
    .expect("production catalog");
    let orchestrator = catalog
        .system_prompt_for(Role::Orchestrator, None, "generic")
        .expect("Orchestrator prompt");
    let advisor = catalog
        .system_prompt_for(Role::Advisor, None, "generic")
        .expect("Advisor prompt");
    let triggers = runtime::prompt::default_role_triggers();
    let guidance = &triggers
        .iter()
        .find(|t| t.name == "Advisor")
        .unwrap()
        .description;
    for trigger in [
        "再レビュー",
        "食い違",
        "仕様では決まらない",
        "仮説を絞りきれない",
    ] {
        assert!(guidance.contains(trigger), "keyTriggers: {trigger}");
        assert_eq!(
            orchestrator.matches(trigger).count(),
            1,
            "Orchestrator prompt repeats {trigger}"
        );
        assert!(advisor.contains(trigger), "Advisor prompt: {trigger}");
    }
    assert!(guidance.contains("権限"));
    // 読み取り専用のため、原因の相談では実行せず次の確認を提案する。
    for contract in ["仮説を見分ける", "実行しない", "権限"] {
        assert!(advisor.contains(contract), "Advisor prompt: {contract}");
    }
}
