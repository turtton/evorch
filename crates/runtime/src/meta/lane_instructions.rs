//! Public lead tools that add to a lane's conversation. The caller identity comes only
//! from the dispatcher; the schema is fixed per run and never varies with lane state.
use super::{DispatchResult, error, parse, serialize, success};
use crate::agent_loop::LoopState;
use crate::lane_instructions::{LaneInstructionAction, LaneInstructionRequest};
use event_bus::LaneBrief;
use providers::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use storage::lane_instructions::LANE_SUPPLEMENT_MAX_BYTES;

/// Most lanes one `lane_send` call may address. The schema and the handler share it.
pub(super) const SEND_MAX_TARGETS: usize = 32;
/// The storage limit on an operation ID. `lane_send` derives one per lane, so the
/// derived ID is what must fit.
const OPERATION_ID_MAX_BYTES: usize = 256;
/// A lane's error text is cut to this many UTF-8 bytes in the result.
const LANE_ERROR_MESSAGE_MAX_BYTES: usize = 1024;
const INSTRUCTION_NOTE: &str = "Accepted means durably stored, not read: the lane receives the full text at its next safe conversation boundary (a finished lane is woken under the existing wake guard; an explicitly stopped lane keeps it pending). It reaches the lane as an agent-authored message, never as user input or permission.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SendArgs {
    operation_id: String,
    message: String,
    targets: Vec<SendTarget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SendTarget {
    lane_id: String,
    expected_management_revision: u64,
    expected_brief_revision: u64,
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

fn input_error(code: &str, message: impl std::fmt::Display) -> String {
    json!({"code":code,"message":message.to_string()}).to_string()
}

/// The ID rule storage applies to every ID, checked here so that nothing is accepted
/// when a later target would be refused for its shape.
fn check_id(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > OPERATION_ID_MAX_BYTES {
        return Err(input_error(
            "invalid_input",
            format!("{field} must be non-blank and at most {OPERATION_ID_MAX_BYTES} UTF-8 bytes"),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(input_error(
            "invalid_input",
            format!("{field} must not contain control characters"),
        ));
    }
    Ok(())
}

/// The one ID a lane's instruction is stored under: stable across retries, target order
/// and the number of targets.
fn lane_operation_id(operation_id: &str, lane_id: &str) -> String {
    format!("{operation_id}:{lane_id}")
}

/// Everything that can be checked without touching a lane, before the first one is.
pub(super) fn validate_send(args: &SendArgs) -> Result<(), String> {
    check_id("operation_id", &args.operation_id)?;
    if args.message.trim().is_empty() || args.message.len() > LANE_SUPPLEMENT_MAX_BYTES {
        return Err(input_error(
            "invalid_input",
            format!(
                "message must be non-blank and at most {LANE_SUPPLEMENT_MAX_BYTES} UTF-8 bytes"
            ),
        ));
    }
    if args.targets.is_empty() || args.targets.len() > SEND_MAX_TARGETS {
        return Err(input_error(
            "invalid_input",
            format!("targets must hold 1 to {SEND_MAX_TARGETS} entries"),
        ));
    }
    let mut seen = HashSet::new();
    for target in &args.targets {
        check_id("lane_id", &target.lane_id)?;
        if target.expected_management_revision == 0 || target.expected_brief_revision == 0 {
            return Err(input_error("invalid_input", "revisions start at 1"));
        }
        if !seen.insert(target.lane_id.as_str()) {
            return Err(input_error(
                "duplicate_lane_id",
                format!("lane {} appears more than once in targets", target.lane_id),
            ));
        }
        if lane_operation_id(&args.operation_id, &target.lane_id).len() > OPERATION_ID_MAX_BYTES {
            return Err(input_error(
                "derived_operation_id_too_long",
                format!(
                    "operation_id, \":\" and lane_id together must be at most {OPERATION_ID_MAX_BYTES} UTF-8 bytes (lane {})",
                    target.lane_id
                ),
            ));
        }
    }
    Ok(())
}

/// Cuts at a character boundary and marks the cut; the result is never longer than `max`.
fn truncate_utf8(text: &str, max: usize) -> String {
    const MARK: &str = "…";
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max.saturating_sub(MARK.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARK}", &text[..end])
}

fn lane_error_result(lane_id: &str, code: &str, message: &str) -> Value {
    json!({
        "lane_id": lane_id,
        "status": "error",
        "error": {"code": code, "message": truncate_utf8(message, LANE_ERROR_MESSAGE_MAX_BYTES)},
    })
}

fn send_body(operation_id: &str, results: Vec<Value>) -> Value {
    json!({"operation_id": operation_id, "results": results, "note": INSTRUCTION_NOTE})
}

fn send_supplement(
    state: &mut LoopState,
    runtime: &crate::AgentRuntime,
    input: Value,
) -> DispatchResult {
    let args: SendArgs = match parse(input) {
        Ok(args) => args,
        Err(reason) => return error(reason),
    };
    if let Err(reason) = validate_send(&args) {
        return error(reason);
    }
    // Each lane commits on its own through the single-lane path; a failure does not undo
    // or skip the others. The same call again settles what is left: a lane that already
    // accepted answers from its receipt.
    let mut accepted = 0;
    let mut results = Vec::with_capacity(args.targets.len());
    for target in args.targets {
        let lane_id = target.lane_id.clone();
        let request = LaneInstructionRequest {
            operation_id: lane_operation_id(&args.operation_id, &lane_id),
            lane_id: target.lane_id,
            expected_management_revision: target.expected_management_revision,
            expected_brief_revision: target.expected_brief_revision,
            action: LaneInstructionAction::Supplement {
                message: args.message.clone(),
            },
        };
        match runtime.send_lane_instruction(state.caller_run_id(), request) {
            Ok(outcome) => {
                accepted += 1;
                results.push(json!({
                    "lane_id": lane_id,
                    "status": "accepted",
                    "operation": outcome.operation,
                    "delivery": outcome.delivery,
                }));
            }
            Err(reason) => results.push(lane_error_result(&lane_id, reason.code, &reason.message)),
        }
    }
    let body = send_body(&args.operation_id, results).to_string();
    // A call that stored nothing is an error; a partial one is not.
    if accepted == 0 {
        error(body)
    } else {
        success(body)
    }
}

pub(super) fn send(
    state: &mut LoopState,
    runtime: &crate::AgentRuntime,
    name: &str,
    input: Value,
) -> DispatchResult {
    if name == "lane_send" {
        return send_supplement(state, runtime, input);
    }
    let args: ReviseArgs = match parse(input) {
        Ok(args) => args,
        Err(reason) => return error(reason),
    };
    let request = LaneInstructionRequest {
        operation_id: args.operation_id,
        lane_id: args.lane_id,
        expected_management_revision: args.expected_management_revision,
        expected_brief_revision: args.expected_brief_revision,
        action: LaneInstructionAction::ReviseBrief { brief: args.brief },
    };
    match runtime.send_lane_instruction(state.caller_run_id(), request) {
        Ok(outcome) => serialize(&json!({
            "status": if outcome.operation.changed { "accepted" } else { "unchanged" },
            "operation": outcome.operation,
            "delivery": outcome.delivery,
            "note": INSTRUCTION_NOTE,
        })),
        Err(reason) => error(json!({"code":reason.code,"message":reason.message}).to_string()),
    }
}

fn send_spec(name: &str) -> ToolSpec {
    let properties = json!({
        "operation_id":{
            "type":"string","minLength":1,"maxLength":OPERATION_ID_MAX_BYTES,
            "description":format!("Identifies this send for retries. Each lane stores it as operation_id + \":\" + lane_id, which must be at most {OPERATION_ID_MAX_BYTES} UTF-8 bytes, so keep it short. Reuse it only to retry this same call.")
        },
        "message":{
            "type":"string","minLength":1,"maxLength":LANE_SUPPLEMENT_MAX_BYTES,
            "description":format!("The supplement text, the same for every target. At most {LANE_SUPPLEMENT_MAX_BYTES} UTF-8 bytes.")
        },
        "targets":{
            "type":"array","minItems":1,"maxItems":SEND_MAX_TARGETS,
            "description":format!("The lanes to send to, 1 to {SEND_MAX_TARGETS}, each lane at most once. Use the observed revisions from lane_list."),
            "items":{
                "type":"object",
                "properties":{
                    "lane_id":{"type":"string","minLength":1,"maxLength":OPERATION_ID_MAX_BYTES},
                    "expected_management_revision":{"type":"integer","minimum":1},
                    "expected_brief_revision":{"type":"integer","minimum":1}
                },
                "required":["lane_id","expected_management_revision","expected_brief_revision"],
                "additionalProperties":false
            }
        }
    });
    super::specs::object_spec(
        name,
        "Send one supplement to one or more lanes you currently lead (attached or summary, already started). It is appended to each lane's conversation as an agent-authored message; the brief and its revision stay unchanged, so earlier Ready reports and close evidence remain valid. Never use it to change objectives or acceptance criteria: use lane_revise_brief. Only a lane's current lane-external root Orchestrator lead may send to it; a detached lane or an old lead is rejected. Every lane is accepted or refused on its own and the result lists each in order: a refusal never undoes or blocks the others, and a call that stored nothing is an error. To settle the lanes that failed, correct the cause (for example a stale revision) and repeat the call: lanes that already accepted answer from their stored result and receive nothing twice, and changing a lane's revisions or the message for an already accepted lane is rejected. Saved durably before it is delivered; it is not read-confirmation and is never user input or permission.",
        properties,
        &["operation_id", "message", "targets"],
        true,
    )
}

pub(super) fn spec(name: &str) -> ToolSpec {
    if name == "lane_send" {
        return send_spec(name);
    }
    let mut properties = json!({
        "operation_id":{"type":"string","minLength":1,"maxLength":256},
        "lane_id":{"type":"string","minLength":1,"maxLength":256},
        "expected_management_revision":{"type":"integer","minimum":1},
        "expected_brief_revision":{"type":"integer","minimum":1}
    });
    let required = [
        "operation_id",
        "lane_id",
        "expected_management_revision",
        "expected_brief_revision",
        "brief",
    ];
    properties["brief"] = super::lanes::brief_schema();
    super::specs::object_spec(
        name,
        "Replace the whole brief of a lane you currently lead (not a patch: send every field). report_policy must equal the lane's current policy (use lane_set_policy to change it). A changed brief raises the brief revision, so earlier Ready reports, verification and close evidence stop supporting a close; an identical brief changes nothing. The lane receives it in its conversation as an agent-authored brief revision, never as user input or permission, and workspace declarations grant no permissions. Only the lane's current lane-external root Orchestrator lead may call this; a detached lane or an old lead is rejected. Use the observed management and brief revisions (lane_list) and a unique operation_id; an identical retry replays the original result.",
        properties,
        &required,
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lane_instructions::{LaneInstructionDelivery, LaneInstructionResult};

    fn args(operation_id: &str, lane_ids: &[&str]) -> SendArgs {
        serde_json::from_value(json!({
            "operation_id": operation_id,
            "message": "supplement",
            "targets": lane_ids.iter().map(|lane| json!({
                "lane_id": lane,
                "expected_management_revision": 1,
                "expected_brief_revision": 1,
            })).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn code(rejected: Result<(), String>) -> String {
        let body: Value = serde_json::from_str(&rejected.unwrap_err()).unwrap();
        body["code"].as_str().unwrap().to_owned()
    }

    #[test]
    fn target_count_is_one_to_the_shared_maximum() {
        let lanes: Vec<String> = (0..=SEND_MAX_TARGETS)
            .map(|n| format!("lane-{n}"))
            .collect();
        let refs = |count: usize| {
            lanes[..count]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        };
        assert!(validate_send(&args("op", &refs(1))).is_ok());
        assert!(validate_send(&args("op", &refs(SEND_MAX_TARGETS))).is_ok());
        assert_eq!(code(validate_send(&args("op", &[]))), "invalid_input");
        assert_eq!(
            code(validate_send(&args("op", &refs(SEND_MAX_TARGETS + 1)))),
            "invalid_input"
        );
    }

    #[test]
    fn a_lane_may_appear_once_whatever_its_revisions() {
        let mut duplicated = args("op", &["a", "b", "a"]);
        duplicated.targets[2].expected_brief_revision = 9;
        assert_eq!(code(validate_send(&duplicated)), "duplicate_lane_id");
    }

    #[test]
    fn the_derived_operation_id_is_checked_in_utf8_bytes_for_every_target() {
        // 256 bytes exactly: "é" is two bytes.
        let lane = "é".repeat(100);
        let operation = "o".repeat(OPERATION_ID_MAX_BYTES - lane.len() - 1);
        assert!(validate_send(&args(&operation, &[&lane])).is_ok());
        assert_eq!(
            lane_operation_id(&operation, &lane).len(),
            OPERATION_ID_MAX_BYTES
        );
        let longer = format!("{operation}o");
        assert_eq!(
            code(validate_send(&args(&longer, &["short", &lane]))),
            "derived_operation_id_too_long",
            "a later target past the limit refuses the whole call"
        );
        // Characters are not bytes: 129 two-byte characters exceed 256 bytes.
        assert_eq!(
            code(validate_send(&args(&"é".repeat(129), &["a"]))),
            "invalid_input"
        );
    }

    #[test]
    fn identifiers_and_message_are_checked_before_any_lane() {
        for operation in ["", "  ", "bad\nid"] {
            assert_eq!(
                code(validate_send(&args(operation, &["a"]))),
                "invalid_input",
                "{operation:?}"
            );
        }
        assert_eq!(code(validate_send(&args("op", &[" "]))), "invalid_input");
        let mut blank = args("op", &["a"]);
        blank.message = " \n".into();
        assert_eq!(code(validate_send(&blank)), "invalid_input");
        let mut long = args("op", &["a"]);
        long.message = "m".repeat(LANE_SUPPLEMENT_MAX_BYTES + 1);
        assert_eq!(code(validate_send(&long)), "invalid_input");
        long.message = "m".repeat(LANE_SUPPLEMENT_MAX_BYTES);
        assert!(validate_send(&long).is_ok());
        let mut zero = args("op", &["a"]);
        zero.targets[0].expected_management_revision = 0;
        assert_eq!(code(validate_send(&zero)), "invalid_input");
    }

    #[test]
    fn truncation_keeps_the_limit_and_character_boundaries() {
        assert_eq!(truncate_utf8("short", 16), "short");
        let text = "あ".repeat(500);
        for max in [4, 5, 6, 7, 100, 1024] {
            let cut = truncate_utf8(&text, max);
            assert!(cut.len() <= max, "{max}: {}", cut.len());
            assert!(cut.ends_with('…'));
            assert!(text.starts_with(cut.trim_end_matches('…')));
        }
    }

    /// The largest body the handler can return, derived from the limits it enforces:
    /// every entry is bounded by its identifiers and error text, plus one note.
    fn body_bound() -> usize {
        // lane_id twice (entry and operation), the derived ID, the bounded error text
        // and the fixed keys, quotes and separators of one entry.
        let entry = 3 * OPERATION_ID_MAX_BYTES + LANE_ERROR_MESSAGE_MAX_BYTES + 512;
        SEND_MAX_TARGETS * entry + INSTRUCTION_NOTE.len() + OPERATION_ID_MAX_BYTES + 256
    }

    #[test]
    fn the_largest_failed_batch_stays_within_the_bound_with_every_lane_listed() {
        let lane = "あ".repeat(OPERATION_ID_MAX_BYTES / 3);
        let results = (0..SEND_MAX_TARGETS)
            .map(|n| {
                lane_error_result(
                    &format!("{n:02}{lane}"),
                    "stale_management_revision",
                    &"長い説明".repeat(2000),
                )
            })
            .collect::<Vec<_>>();
        let body = send_body(&"o".repeat(OPERATION_ID_MAX_BYTES), results).to_string();
        assert!(
            body.len() <= body_bound(),
            "{} > {}",
            body.len(),
            body_bound()
        );
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed["results"].as_array().unwrap().len(),
            SEND_MAX_TARGETS
        );
        for result in parsed["results"].as_array().unwrap() {
            let message = result["error"]["message"].as_str().unwrap();
            assert!(message.len() <= LANE_ERROR_MESSAGE_MAX_BYTES);
            assert_eq!(result["error"]["code"], "stale_management_revision");
        }
    }

    #[test]
    fn the_largest_accepted_batch_stays_within_the_bound() {
        let lane = "l".repeat(OPERATION_ID_MAX_BYTES);
        let operation_id = "o".repeat(OPERATION_ID_MAX_BYTES);
        let results = (0..SEND_MAX_TARGETS)
            .map(|_| {
                json!({
                    "lane_id": lane,
                    "status": "accepted",
                    "operation": LaneInstructionResult {
                        operation_id: operation_id.clone(),
                        lane_id: lane.clone(),
                        instruction_seq: Some(u64::MAX),
                        notification_id: Some("n".repeat(64)),
                        accepted_management_revision: u64::MAX,
                        brief_revision: u64::MAX,
                        changed: true,
                        replayed: true,
                    },
                    "delivery": LaneInstructionDelivery::Queued,
                })
            })
            .collect::<Vec<_>>();
        let body = send_body(&operation_id, results).to_string();
        assert!(
            body.len() <= body_bound(),
            "{} > {}",
            body.len(),
            body_bound()
        );
    }
}
