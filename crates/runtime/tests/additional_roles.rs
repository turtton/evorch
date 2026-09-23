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
    assert!(researcher_prompt.contains("web_search / web_fetch"));
    assert!(researcher_prompt.contains("出典 URL"));
    let planner_prompt = catalog
        .system_prompt_for(Role::Planner, None, "generic")
        .expect("Planner prompt");
    let oracle_prompt = catalog
        .system_prompt_for(Role::Oracle, None, "generic")
        .expect("Oracle prompt");
    let looker_prompt = catalog
        .system_prompt_for(Role::MultimodalLooker, None, "generic")
        .expect("MultimodalLooker prompt");
    assert_ne!(planner_prompt, oracle_prompt);
    assert_ne!(planner_prompt, looker_prompt);
    assert_ne!(oracle_prompt, looker_prompt);

    // Then: each role selects its own capability-bearing trigger source.
    for role in [
        Role::WebResearcher,
        Role::Planner,
        Role::Oracle,
        Role::MultimodalLooker,
    ] {
        let trigger = triggers
            .iter()
            .find(|trigger| trigger.name == role.name())
            .expect("role trigger");
        assert!(trigger.description.contains("許可ツール:"));
        assert!(trigger.description.contains("ネットワーク:"));
    }
}

#[tokio::test]
async fn delegate_spawns_additional_roles_and_rejects_unknown_names() {
    // Given: a coordinator requesting all new roles and an invalid one.
    let model = Arc::new(ScriptedModel::new([]));
    model.add_keyed("ROOT", [
        Ok(tool_response("researcher", "delegate", json!({"role":"web_researcher", "prompt":"CHILD"}))),
        Ok(tool_response("planner", "delegate", json!({"role":"Planner", "prompt":"CHILD"}))),
        Ok(tool_response("oracle", "delegate", json!({"role":"Oracle", "prompt":"CHILD"}))),
        Ok(tool_response("looker", "delegate", json!({"role":"MultimodalLooker", "prompt":"CHILD", "images":[{"media_type":"image/png", "data":"aGVsbG8="}]}))),
        Ok(tool_response("invalid", "delegate", json!({"role":"invalid", "prompt":"CHILD"}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]).await;
    model
        .add_keyed(
            "CHILD",
            (0..4).map(|_| Ok(text_response("done", FinishReason::Stop))),
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
    assert_eq!(runs.len(), 5);
    for name in ["WebResearcher", "Planner", "Oracle", "MultimodalLooker"] {
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
                let ToolResultContent::Text { text } = &content[0];
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
