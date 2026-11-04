//! Model-facing lane operations. Phase 1b deliberately has no wake/outbox path.
use super::{DispatchResult, error, parse, serialize};
use crate::{AgentRuntime, agent_loop::LoopState, lanes::LaneError};
use event_bus::{Event, LaneBrief, LanePolicy, LaneReportInput, LaneReportProducer, LaneStatus};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    brief: LaneBrief,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    filter: Option<LanePolicy>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    20
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    status: LaneStatus,
    summary: String,
    #[serde(default)]
    artifact_refs: Vec<String>,
    #[serde(default)]
    needs_human: bool,
    #[serde(default)]
    question_refs: Vec<String>,
    #[serde(default)]
    reason_code: Option<String>,
}
fn failure(e: LaneError) -> DispatchResult {
    error(json!({"code":e.code,"message":e.message}).to_string())
}
pub(super) fn open(state: &mut LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    let args: OpenArgs = match parse(input) {
        Ok(a) => a,
        Err(e) => return failure(LaneError::new("invalid_brief", e)),
    };
    let Some(key) = state.meta_call_id.as_deref() else {
        return failure(LaneError::new(
            "lane_creation_denied",
            "missing tool call identity",
        ));
    };
    let key = format!("{}:{key}", state.caller_run_id());
    match runtime.open_lane(state.caller_run_id(), &key, args.brief) {
        Ok(opened) => {
            // Pending admission is not progress; the prepared-context
            // registration delivers identified credit through publish_budget.
            serialize(&opened)
        }
        Err(e) => failure(e),
    }
}
pub(super) fn list(state: &mut LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    let args: ListArgs = match parse(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    let (thread, _) = match runtime.lane_lead(state.caller_run_id()) {
        Ok(a) => a,
        Err(e) => return failure(e),
    };
    let Some(store) = runtime.shared.run_store.get() else {
        return failure(LaneError::new(
            "storage_unavailable",
            "lane ledger is not configured",
        ));
    };
    match store.handle.lane_list(storage::lanes::LaneListRequest {
        lead_thread_id: thread,
        policy: args.filter,
        cursor: args.cursor,
        limit: args.limit,
    }) {
        Ok(page) => {
            // Only data actually returned to this model can acknowledge completion.
            // A new sequence/heartbeat or repeated snapshot cannot mint credits.
            for item in &page.items {
                if let Some(report) = &item.latest_report
                    && matches!(report.status, LaneStatus::Ready | LaneStatus::Failed)
                    && report.brief_revision == item.brief_revision
                    && report.management_revision == item.management_revision
                {
                    state.budget.mark_lane_completed(&item.lane_id);
                }
            }
            serialize(&page)
        }
        Err(e) => failure(e.into()),
    }
}
pub(super) fn report(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> DispatchResult {
    let args: ReportArgs = match parse(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    if state.task.parent.is_some()
        || state.run_role() != agents::Role::Orchestrator
        || state.task.config.lane_scope.is_none()
    {
        return failure(LaneError::new(
            "lane_unavailable",
            "only a lane root may report",
        ));
    }
    if let Err(e) = runtime.validate_run_mutation(state.caller_run_id()) {
        return failure(LaneError::new("lane_unavailable", e));
    }
    let Some(store) = runtime.shared.run_store.get() else {
        return failure(LaneError::new(
            "storage_unavailable",
            "lane ledger is not configured",
        ));
    };
    let Some(thread) = state.thread_id.as_deref() else {
        return failure(LaneError::new("lane_unavailable", "no lane thread"));
    };
    let lane = match store.handle.lane_by_thread(thread) {
        Ok(Some(lane)) => lane,
        Ok(None) => return failure(LaneError::new("lane_unavailable", "lane record is missing")),
        Err(e) => return failure(e.into()),
    };
    if lane.root_run_id.as_deref() != Some(&state.caller_run_id().to_string())
        || lane.policy == LanePolicy::Detached
    {
        return failure(LaneError::new(
            "lane_unavailable",
            "not the current managed lane root",
        ));
    }
    let report = LaneReportInput {
        status: args.status,
        summary: args.summary,
        artifact_refs: args.artifact_refs,
        needs_human: args.needs_human,
        question_refs: args.question_refs,
        reason_code: args.reason_code,
        producer: LaneReportProducer::LaneAgent,
    };
    if let Err(e) = report.validate() {
        return failure(LaneError::new("invalid_report", e));
    }
    match store
        .handle
        .append_lane_report(&lane.lane_id, report, crate::run_store::review_time_ns())
    {
        Ok(appended) => {
            runtime
                .shared
                .bus
                .emit(Event::new(appended.lane.report_appended_event(
                    &appended.report,
                    Some(state.caller_run_id().to_string()),
                )));
            serialize(&appended.report)
        }
        Err(e) => failure(e.into()),
    }
}

fn object(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> providers::ToolSpec {
    providers::ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    }
}
fn strings() -> Value {
    json!({"type":"array","items":{"type":"string"}})
}
pub(super) fn open_spec(name: &str) -> providers::ToolSpec {
    object(
        name,
        "Open an independent Orchestrator lane from an agent-authored brief; does not transfer this lead's goal or ownership. Workspace declarations grant no permissions. Returns lane/thread/root IDs. Phase 1b preview: questions go to the user under every policy; no automatic lead wake. Use lane_list for reports; ready remains unverified.",
        json!({"brief":{"type":"object","properties":{
        "objective":{"type":"string"},"inputs":{"type":"array","items":{"type":"object","properties":{"kind":{"enum":["description","thread","message","artifact"]},"value":{"type":"string"}},"required":["kind","value"],"additionalProperties":false}},
        "deliverables":strings(),"acceptance_criteria":strings(),"constraints":strings(),
        "workspace":{"type":"object","properties":{"branch":{"type":"string"},"worktree":{"type":"string"},"notes":{"type":"string"}},"additionalProperties":false},
        "report_policy":{"enum":["attached","summary","detached"]}},"required":["objective","inputs","deliverables","acceptance_criteria","constraints","report_policy"],"additionalProperties":false}}),
        &["brief"],
    )
}
pub(super) fn list_spec(name: &str) -> providers::ToolSpec {
    object(
        name,
        "Read currently managed lanes in latest real-activity order (lane ID breaks ties), at most 20 entries/16 KiB. Continue with next_cursor and the same filter; stale_cursor requires a fresh listing. Reports are bounded self-reports, not verified completion. Reads do not update activity or wake anyone.",
        json!({"filter":{"enum":["attached","summary"]},"cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20}}),
        &[],
    )
}
pub(super) fn report_spec(name: &str) -> providers::ToolSpec {
    object(
        name,
        "Append this lane root's bounded structured report (summary <=1 KiB, whole report <=4 KiB, at most 8 artifact refs). ready is an unverified claim, not completion/close. Phase 1b preview: the lead retrieves reports with lane_list; this does not wake it. Detached lanes do not report to a lead.",
        json!({"status":{"enum":["working","waiting","blocked","ready","failed"]},"summary":{"type":"string"},"artifact_refs":strings(),"needs_human":{"type":"boolean"},"question_refs":strings(),"reason_code":{"type":"string"}}),
        &["status", "summary"],
    )
}
