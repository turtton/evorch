//! Public management tools. The caller identity comes only from the dispatcher.
use super::{DispatchResult, error, parse, serialize};
use crate::agent_loop::LoopState;
use crate::lane_management::{
    LaneManagementAction, LaneManagementContext, LaneManagementRequest, ManagedLanePolicy,
};
use providers::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PolicyArgs {
    operation_id: String,
    lane_id: String,
    expected_management_revision: u64,
    policy: ManagedLanePolicy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DetachArgs {
    operation_id: String,
    lane_id: String,
    expected_management_revision: u64,
}

pub(super) fn manage(
    state: &mut LoopState,
    runtime: &crate::AgentRuntime,
    name: &str,
    input: Value,
) -> DispatchResult {
    let request = if name == "lane_set_policy" {
        let args: PolicyArgs = match parse(input) {
            Ok(args) => args,
            Err(reason) => return error(reason),
        };
        LaneManagementRequest {
            operation_id: args.operation_id,
            lane_id: args.lane_id,
            expected_management_revision: args.expected_management_revision,
            action: LaneManagementAction::SetPolicy {
                policy: args.policy,
            },
        }
    } else {
        let args: DetachArgs = match parse(input) {
            Ok(args) => args,
            Err(reason) => return error(reason),
        };
        LaneManagementRequest {
            operation_id: args.operation_id,
            lane_id: args.lane_id,
            expected_management_revision: args.expected_management_revision,
            action: LaneManagementAction::Detach,
        }
    };
    let context = LaneManagementContext::Run(state.caller_run_id());
    let result = if name == "lane_set_policy" {
        runtime.set_lane_policy(context, request)
    } else {
        runtime.detach_lane(context, request)
    };
    match result {
        Ok(outcome) => serialize(&outcome),
        Err(reason) => error(json!({"code":reason.code,"message":reason.message}).to_string()),
    }
}

pub(super) fn spec(name: &str) -> ToolSpec {
    let mut properties = json!({
        "operation_id":{"type":"string","minLength":1,"maxLength":256},
        "lane_id":{"type":"string","minLength":1,"maxLength":256},
        "expected_management_revision":{"type":"integer","minimum":1}
    });
    let mut required = vec!["operation_id", "lane_id", "expected_management_revision"];
    let description = if name == "lane_set_policy" {
        properties["policy"] = json!({"type":"string","enum":["attached","summary"]});
        required.push("policy");
        "Change a managed lane's reporting policy without restarting its run. Only its current lane-external root Orchestrator lead may call this. Detached lanes reject management. Use the observed management revision and a unique operation_id; identical retries replay the original result."
    } else {
        "Detach a lane, ending lead management without closing or stopping it. Only its current lane-external root Orchestrator lead may call this. Reattachment is a human sidebar drag-and-drop only, never a tool. Use the observed management revision and a unique operation_id; identical retries replay the original result."
    };
    super::specs::object_spec(name, description, properties, &required, true)
}
