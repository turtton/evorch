mod support;

use event_bus::AgentRunPhase;
use providers::FinishReason;
use runtime::{Role, RunConfig};
use serde_json::json;
use std::sync::Arc;
use support::{ScriptedModel, text_response, tool_response};

#[tokio::test]
async fn reviewer_submission_is_independent_of_contradictory_final_text() {
    // Given: the reviewer submits typed approval, then writes contradictory prose.
    let typed = json!({"verdict":"approve", "criteria":[{
        "id":"AC1", "status":"met", "note":"checked", "evidence":{
            "command":"cargo test", "exit_status":0, "target_sha":"head",
            "diff_ref":"base..head", "artifact_path":"green.log", "red_evidence":"red.log"
        }
    }]});
    let prose = "```json\n{\"verdict\":\"request-update\",\"findings\":[\"contradiction\"]}\n```";
    let model = ScriptedModel::new([
        Ok(tool_response("review", "submit_review", typed)),
        Ok(text_response(prose, FinishReason::Stop)),
    ]);
    let bus = Arc::new(event_bus::EventBus::new(64));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = runtime::AgentRuntime::new(bus, executor, Arc::new(model));
    let run = runtime.delegate_background(Role::Reviewer, "review".into(), RunConfig::default());
    // When
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    // Then: the dedicated channel retains approval, not the final text's verdict.
    assert_eq!(runtime.run_result(run).unwrap().as_deref(), Some(prose));
    assert_eq!(
        runtime.reviewer_result(run).unwrap().verdict,
        event_bus::ReviewVerdict::Approve
    );
}
