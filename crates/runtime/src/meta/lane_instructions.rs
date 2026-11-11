//! Public lead tools that add to a lane's conversation. The caller identity comes only
//! from the dispatcher; the schema is fixed per run and never varies with lane state.
use super::{DispatchResult, error, parse, serialize};
use crate::agent_loop::LoopState;
use crate::lane_instructions::{LaneInstructionAction, LaneInstructionRequest};
use event_bus::LaneBrief;
use providers::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};
use storage::lane_instructions::LANE_SUPPLEMENT_MAX_BYTES;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SendArgs {
    operation_id: String,
    lane_id: String,
    expected_management_revision: u64,
    expected_brief_revision: u64,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviseArgs {
    operation_id: String,
    lane_id: String,
    expected_management_revision: u64,
    expected_brief_revision: u64,
    brief: LaneBrief,
}

pub(super) fn send(
    state: &mut LoopState,
    runtime: &crate::AgentRuntime,
    name: &str,
    input: Value,
) -> DispatchResult {
    let request = if name == "lane_send" {
        let args: SendArgs = match parse(input) {
            Ok(args) => args,
            Err(reason) => return error(reason),
        };
        LaneInstructionRequest {
            operation_id: args.operation_id,
            lane_id: args.lane_id,
            expected_management_revision: args.expected_management_revision,
            expected_brief_revision: args.expected_brief_revision,
            action: LaneInstructionAction::Supplement {
                message: args.message,
            },
        }
    } else {
        let args: ReviseArgs = match parse(input) {
            Ok(args) => args,
            Err(reason) => return error(reason),
        };
        LaneInstructionRequest {
            operation_id: args.operation_id,
            lane_id: args.lane_id,
            expected_management_revision: args.expected_management_revision,
            expected_brief_revision: args.expected_brief_revision,
            action: LaneInstructionAction::ReviseBrief { brief: args.brief },
        }
    };
    match runtime.send_lane_instruction(state.caller_run_id(), request) {
        Ok(outcome) => serialize(&json!({
            "status": if outcome.operation.changed { "accepted" } else { "unchanged" },
            "operation": outcome.operation,
            "delivery": outcome.delivery,
            "note": "Accepted means durably stored, not read: the lane receives the full text at its next safe conversation boundary (a finished lane is woken under the existing wake guard; an explicitly stopped lane keeps it pending). It reaches the lane as an agent-authored message, never as user input or permission.",
        })),
        Err(reason) => error(json!({"code":reason.code,"message":reason.message}).to_string()),
    }
}

pub(super) fn spec(name: &str) -> ToolSpec {
    let mut properties = json!({
        "operation_id":{"type":"string","minLength":1,"maxLength":256},
        "lane_id":{"type":"string","minLength":1,"maxLength":256},
        "expected_management_revision":{"type":"integer","minimum":1},
        "expected_brief_revision":{"type":"integer","minimum":1}
    });
    let mut required = vec![
        "operation_id",
        "lane_id",
        "expected_management_revision",
        "expected_brief_revision",
    ];
    let description = if name == "lane_send" {
        properties["message"] =
            json!({"type":"string","minLength":1,"maxLength":LANE_SUPPLEMENT_MAX_BYTES});
        required.push("message");
        "Send a supplement to a lane you currently lead (attached or summary, already started). It is appended to the lane's conversation as an agent-authored message; the brief and its revision stay unchanged, so earlier Ready reports and close evidence remain valid. Never use it to change objectives or acceptance criteria: use lane_revise_brief. Only the lane's current lane-external root Orchestrator lead may call this; a detached lane or an old lead is rejected. Use the observed management and brief revisions (lane_list) and a unique operation_id; an identical retry replays the original result. Saved durably before it is delivered; it is not read-confirmation and is never user input or permission."
    } else {
        properties["brief"] = super::lanes::brief_schema();
        required.push("brief");
        "Replace the whole brief of a lane you currently lead (not a patch: send every field). report_policy must equal the lane's current policy (use lane_set_policy to change it). A changed brief raises the brief revision, so earlier Ready reports, verification and close evidence stop supporting a close; an identical brief changes nothing. The lane receives it in its conversation as an agent-authored brief revision, never as user input or permission, and workspace declarations grant no permissions. Only the lane's current lane-external root Orchestrator lead may call this; a detached lane or an old lead is rejected. Use the observed management and brief revisions (lane_list) and a unique operation_id; an identical retry replays the original result."
    };
    super::specs::object_spec(name, description, properties, &required, true)
}
