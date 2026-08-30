mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::FinishReason;
use runtime::{AgentRuntime, AgentSummary, RunConfig, RunId};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

fn runtime_with(model: Arc<ScriptedModel>) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, model)
}

#[tokio::test]
async fn list_agents_reports_name_role_and_model() {
    // Given: marker ごとに 1 回の Stop 応答を返す keyed script と共有 EventBus の runtime
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed("ORCH", [Ok(text_response("done", FinishReason::Stop))])
        .await;
    model
        .add_keyed("W", [Ok(text_response("done", FinishReason::Stop))])
        .await;
    let runtime = runtime_with(Arc::clone(&model));

    // When: Orchestrator は既定 config、Worker は表示名付き config で background 実行する
    let orchestrator =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());
    let worker = runtime.delegate_background(
        Role::Worker,
        "W".to_string(),
        RunConfig {
            name: Some("worker-w1".to_string()),
            ..RunConfig::default()
        },
    );
    assert_eq!(runtime.wait(orchestrator).await, Ok(AgentRunPhase::Done));
    assert_eq!(runtime.wait(worker).await, Ok(AgentRunPhase::Done));

    // Then: 一覧は run id 順で、name / role_name / model が行ごとに区別できる値を持つ
    let summaries = runtime.list_agents();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].name, "Orchestrator");
    assert_eq!(summaries[0].role_name, "Orchestrator");
    assert_eq!(summaries[0].model, "scripted-orchestrator");
    assert_eq!(summaries[0].phase, AgentRunPhase::Done);
    assert_eq!(summaries[1].name, "worker-w1");
    assert_eq!(summaries[1].role_name, "Worker");
    assert_eq!(summaries[1].model, "scripted-worker");
    assert_eq!(summaries[1].phase, AgentRunPhase::Done);
}

// Given: name / role_name / model がすべて異なる値の AgentSummary / When: JSON 化 /
// Then: identity 項目が契約どおりのキーで現れ、run_id は数値として serialize される
#[test]
fn agent_summary_serializes_identity_fields() {
    let summary = AgentSummary {
        run_id: RunId::new(9),
        name: "ops-lead".to_string(),
        role_name: "Reviewer".to_string(),
        phase: AgentRunPhase::Done,
        model: "model-z".to_string(),
    };

    let json = serde_json::to_value(&summary).expect("serialize AgentSummary");

    assert_eq!(json["name"], json!("ops-lead"));
    assert_eq!(json["role_name"], json!("Reviewer"));
    assert_eq!(json["model"], json!("model-z"));
    assert_eq!(json["phase"], json!("Done"));
    assert!(json["run_id"].is_number());
}
