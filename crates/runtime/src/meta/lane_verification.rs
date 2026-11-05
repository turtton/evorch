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
    verification_ref: String,
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
            serialize(
                &json!({"verification_ref":v.verification_id.to_string(),"state":v.state,"reviewer_run_id":v.reviewer_run_id}),
            )
        }
        Err(e) => failure(e),
    }
}
pub(super) async fn close(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> DispatchResult {
    let args: Close = match parse(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    let id = match parse_id(&args.verification_ref) {
        Ok(id) => id,
        Err(e) => return failure(e),
    };
    match runtime
        .close_verified_lane(state.caller_run_id(), &args.lane_id, args.ready_seq, id)
        .await
    {
        Ok(result) => serialize(
            &json!({"lane_id":result.lane_id,"close_id":result.close_id,"verification_ref":result.verification_ref.to_string(),"ready_seq":result.ready_seq,"closed_revision":result.closed_revision,"outcome":result.outcome}),
        ),
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
            "Request an independent read-only Reviewer for the current ready report of a lane you manage. Explicitly starts a normal child and uses delegate spawn credit. Requires an immutable artifact revision: SHA-256 of exactly one regular file (<=64 MiB, artifact_refs path within allowed directories), or a clean uniquely resolved commit checkout. verification_requirements.pull_request=true requires Fresh structured facts of the lane's own PR (bounded refresh here); CI green is not a Reviewer verdict. Returns verification_ref/reviewer_run_id; wait for that child. Ready is not closed.",
            target,
            vec!["lane_id", "ready_seq"],
        ),
        "lane_close" => {
            let mut p = target;
            p["verification_ref"] = json!({"type":"string","pattern":"^[0-9a-f]{32}$"});
            (
                "Close only with an approved structured submission AND normal termination by the assigned independent Reviewer. Rechecks current ready/brief/requests/artifact and Fresh required PR facts; never fetches here. Stale or incomplete evidence rejects. Idempotent AlreadyClosed returns the original close ID/evidence; new lane inputs/reports reopen current work, without changing past closes. Does not detach/stop/delete the lane.",
                p,
                vec!["lane_id", "ready_seq", "verification_ref"],
            )
        }
        "submit_lane_verification" => (
            "Submit exactly once for your runtime-assigned verification_ref and finish the review. Check every zero-based criterion using actual evidence. Approve only when every check passes, evidence_refs are nonempty and findings are empty; otherwise reject with concrete findings. Only the assigned Reviewer purpose may submit, not a lead, Worker, Advisor or another Reviewer. Submission alone does not close the lane; normal run completion is also required.",
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
