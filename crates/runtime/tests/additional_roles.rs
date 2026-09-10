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
    for role in [Role::Planner, Role::Oracle, Role::MultimodalLooker] {
        assert!(catalog.system_prompt_for(role, None, "generic").is_ok());
        assert!(triggers.iter().any(|trigger| trigger.name == role.name()));
    }
}

#[tokio::test]
async fn delegate_spawns_additional_roles_and_rejects_unknown_names() {
    // Given: a coordinator requesting all new roles and an invalid one.
    let model = Arc::new(ScriptedModel::new([]));
    model.add_keyed("ROOT", [
        Ok(tool_response("planner", "delegate", json!({"role":"Planner", "prompt":"CHILD"}))),
        Ok(tool_response("oracle", "delegate", json!({"role":"Oracle", "prompt":"CHILD"}))),
        Ok(tool_response("looker", "delegate", json!({"role":"MultimodalLooker", "prompt":"CHILD", "images":[{"media_type":"image/png", "data":"aGVsbG8="}]}))),
        Ok(tool_response("invalid", "delegate", json!({"role":"invalid", "prompt":"CHILD"}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]).await;
    model
        .add_keyed(
            "CHILD",
            (0..3).map(|_| Ok(text_response("done", FinishReason::Stop))),
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
    assert_eq!(runs.len(), 4);
    for name in ["Planner", "Oracle", "MultimodalLooker"] {
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
            {
                if tool_call_id == "invalid" {
                    let ToolResultContent::Text { text } = &content[0];
                    return Some(
                        serde_json::from_str::<serde_json::Value>(text).expect("structured error"),
                    );
                }
            }
            None
        })
        .expect("unknown role result");
    assert_eq!(error["code"], "unknown_role");
    assert_eq!(error["role"], "invalid");
}
