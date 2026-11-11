//! Model-facing lane verification tools. Caller/role/purpose are runtime facts.
use super::{DispatchResult, Terminal, error, parse, serialize};
use crate::lane_verification::{operation, parse_id, storage_error};
use crate::{AgentRuntime, RunPurpose, agent_loop::LoopState};
use event_bus::lane::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    lane_id: String,
    ready_seq: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Close {
    lane_id: String,
    ready_seq: u64,
    expected_management_revision: u64,
    expected_request_revision: u64,
    note: String,
    #[serde(default)]
    verification_ref: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Submit {
    verification_ref: String,
    verdict: LaneVerificationVerdict,
    checks: Vec<LaneCriterionCheck>,
    evidence_refs: Vec<String>,
    #[serde(default)]
    findings: Vec<String>,
}
fn failure(e: crate::lanes::LaneError) -> DispatchResult {
    error(json!({"error":{"code":e.code,"message":e.message}}).to_string())
}
pub(super) async fn verify(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> DispatchResult {
    let args: Target = match parse(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    match runtime
        .verify_lane(state.caller_run_id(), &args.lane_id, args.ready_seq)
        .await
    {
        Ok(v) => {
            // Normal delegate accounting: only starting a child consumes credit;
            // submit does not replenish it. First observed terminal output does.
            if state.budget.mark_spawned(&v.reviewer_run_id) {
                state.budget.mark_progress();
            }
            serialize(&json!({
                "verification_ref": v.verification_id.to_string(),
                "state": v.state,
                "reviewer_run_id": v.reviewer_run_id,
                // What lane_close compares, as seen when the review started.
                "ready_seq": v.target.ready_seq,
                "management_revision": v.target.management_revision,
                "request_revision": v.target.request_revision,
                "brief_revision": v.target.brief_revision,
                "caveats": v.caveats,
            }))
        }
        Err(e) => failure(e),
    }
}
pub(super) fn close(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    let args: Close = match parse(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    let verification_id = match args.verification_ref.as_deref().map(parse_id).transpose() {
        Ok(id) => id,
        Err(e) => return failure(e),
    };
    match runtime.close_lane(
        state.caller_run_id(),
        crate::lane_close::LaneCloseRequest {
            lane_id: args.lane_id,
            ready_seq: args.ready_seq,
            expected_management_revision: args.expected_management_revision,
            expected_request_revision: args.expected_request_revision,
            note: args.note,
            verification_id,
        },
    ) {
        Ok(result) => serialize(&json!({
            "lane_id": result.lane_id,
            "close_id": result.close_id,
            "kind": result.kind,
            "outcome": result.outcome,
            "ready_seq": result.ready_seq,
            "closed_revision": result.closed_revision,
            "management_revision": result.management_revision,
            "brief_revision": result.brief_revision,
            "artifact_revision": result.artifact_revision,
            "review": result.review.as_ref().map(|r| json!({
                "verification_ref": r.verification_id.to_string(),
                "state": r.state,
                "verdict": r.verdict,
                "targets_this_ready": r.targets_this_ready,
                "caveats": r.caveats.len(),
                "artifact_rechecked_at_close": r.artifact_rechecked_at_close,
            })),
        })),
        Err(e) => failure(e),
    }
}
pub(super) fn submit(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    let RunPurpose::LaneVerification {
        verification_id,
        lead_root,
    } = state.run_config().purpose
    else {
        return failure(operation(LaneOperationError::NotResponder));
    };
    if state.run_role() != agents::Role::Reviewer || state.task.parent != Some(lead_root) {
        return failure(operation(LaneOperationError::NotResponder));
    }
    let args: Submit = match parse(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    if parse_id(&args.verification_ref).ok() != Some(verification_id) {
        return failure(operation(LaneOperationError::NotResponder));
    }
    let body = match serde_json::to_string(&args) {
        Ok(s) if s.len() <= 24 * 1024 => s,
        _ => return failure(operation(LaneOperationError::InvalidInput)),
    };
    if args.evidence_refs.len() > 8
        || args
            .evidence_refs
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 256)
        || args.findings.len() > 32
        || args
            .findings
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 1024)
        || (args.verdict == LaneVerificationVerdict::Approved
            && (!args.findings.is_empty() || args.evidence_refs.is_empty()))
    {
        return failure(operation(LaneOperationError::VerificationRejected));
    }
    if let Err(e) = runtime.validate_run_mutation(state.caller_run_id()) {
        return failure(crate::lanes::LaneError::new("ownership_lost", e));
    }
    let _guard = match state
        .run_config()
        .ownership
        .as_ref()
        .map(|p| p.mutation_guard())
        .transpose()
    {
        Ok(g) => g,
        Err(_) => return failure(operation(LaneOperationError::OwnershipLost)),
    };
    let Some(store) = runtime.shared.run_store.get() else {
        return failure(crate::lanes::LaneError::new(
            "storage_unavailable",
            "verification requires storage",
        ));
    };
    let submission = LaneVerificationSubmission {
        verdict: args.verdict,
        checks: args.checks,
        result_ref: format!("lane-review:{}", state.caller_run_id()),
        result_digest: format!("{:x}", Sha256::digest(body.as_bytes())),
    };
    match store.handle.submit_lane_verification(
        verification_id,
        state.caller_run_id().to_string(),
        submission,
        crate::run_store::review_time_ns(),
    ) {
        Ok(_) => DispatchResult {
            result: tools::ToolResult::success(
                json!({"submitted":true,"verification_ref":args.verification_ref}).to_string(),
            ),
            terminal: Terminal::Finish(format!("[independent lane verification] {body}")),
        },
        Err(e) => failure(storage_error(e)),
    }
}
pub(super) fn spec(name: &str) -> providers::ToolSpec {
    let target = json!({"lane_id":{"type":"string"},"ready_seq":{"type":"integer","minimum":1}});
    let (description, properties, required) = match name {
        "lane_verify" => (
            "Optionally request an independent read-only Reviewer for the current ready report of a lane you manage. Starts a normal child and uses delegate spawn credit. Best effort: a fixed artifact revision is recommended (full commit SHA, or SHA-256 of exactly one regular file <=64 MiB) but not required; when the artifact or the lane's own PR facts cannot be observed or do not match, the review still starts and the observation limits are recorded and passed to the Reviewer. The Reviewer judges only what it can observe. The review never decides whether you may close: it is a reference for your own judgement. Returns verification_ref/reviewer_run_id and the revisions lane_close compares; wait for that child. Ready is not closed.",
            target,
            vec!["lane_id", "ready_seq"],
        ),
        "lane_close" => {
            let mut p = target;
            p["expected_management_revision"] = json!({"type":"integer","minimum":0});
            p["expected_request_revision"] = json!({"type":"integer","minimum":0});
            p["note"] = json!({"type":"string","minLength":1,"maxLength":2048});
            p["verification_ref"] = json!({"type":"string","pattern":"^[0-9a-f]{32}$"});
            (
                "Close a lane you manage on your own judgement, ending its management ledger entry; this is not a system-verified acceptance. Required: you are the lane's current lead, the lane is attached or summary, ready_seq is its latest report and that report is a Ready reported for the lane's current brief (a Ready for an older brief is rejected as stale_brief; ask for a new Ready), and expected_management_revision / expected_request_revision still match. The revisions come from the lane (management_revision from lane_list or lane_verify; request_revision from lane_verify or from the current revisions that a stale_* rejection returns); if you hold none, send 0 and read the current revisions from the rejection, then decide again. note is your reason, kept in the close record. Not required and not checked here: an immutable artifact revision, a clean checkout, fresh PR facts, a Reviewer or its approval; no Git, GitHub or file is read. Optional verification_ref (a lane_verify result of this lane, in any state) is attached for reference only. The record keeps the Ready's artifact_refs/revision as reported, the attached review's state and verdict, and its observation limits; nothing is re-observed at close. Retry with the same arguments returns the same close_id with outcome already_closed; after new lane input, a new report or a brief or management change the close is cleared and the same arguments are rejected as stale_request (never closed again silently). Does not stop, detach or delete the lane. To recommend, not require: put a full commit SHA, PR URL or artifact file in your lane's Ready (lane_report) when you can.",
                p,
                vec![
                    "lane_id",
                    "ready_seq",
                    "expected_management_revision",
                    "expected_request_revision",
                    "note",
                ],
            )
        }
        "submit_lane_verification" => (
            "Submit exactly once for your runtime-assigned verification_ref and finish the review. Check every zero-based criterion of the brief you were started with, using evidence you actually observed. Approve only when every check passes, evidence_refs are nonempty and findings are empty; otherwise reject with concrete findings. Only the assigned Reviewer purpose may submit, not a lead, Worker, Advisor or another Reviewer. Your result is a reference for the lead's own close decision; it does not close the lane.",
            json!({"verification_ref":{"type":"string","pattern":"^[0-9a-f]{32}$"},"verdict":{"enum":["approved","rejected"]},"checks":{"type":"array","maxItems":32,"items":{"type":"object","properties":{"criterion_index":{"type":"integer","minimum":0},"passed":{"type":"boolean"},"evidence_refs":{"type":"array","maxItems":8,"items":{"type":"string","maxLength":256}}},"required":["criterion_index","passed","evidence_refs"],"additionalProperties":false}},"evidence_refs":{"type":"array","maxItems":8,"items":{"type":"string","maxLength":256}},"findings":{"type":"array","maxItems":32,"items":{"type":"string","maxLength":1024}}}),
            vec!["verification_ref", "verdict", "checks", "evidence_refs"],
        ),
        _ => unreachable!(),
    };
    providers::ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    }
}
