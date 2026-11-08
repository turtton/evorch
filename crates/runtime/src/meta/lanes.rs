//! Model-facing lane operations. Delivery hooks fail safe until PR C is connected.
use super::{DispatchResult, error, parse, serialize};
use crate::runtime::questions::routing::{LaneAnswerRequest, LaneQuestionBindingSource};
use crate::{AgentRuntime, agent_loop::LoopState, lanes::LaneError};
use event_bus::lane::ArtifactRevision;
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
    notifications_after: Option<String>,
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
    #[serde(default)]
    artifact_revision: Option<ArtifactRevision>,
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
    if let Some(after) = args.notifications_after {
        if after.len() > 256 || args.cursor.is_some() || args.filter.is_some() {
            return failure(LaneError::new(
                "invalid_arguments",
                "notification mode cannot use lane cursor/filter; after must be <=256 bytes",
            ));
        }
        return match store.handle.lane_inbox(thread, after) {
            Ok(notices) => {
                let next = notices.last().map(|n| n.notification_id.clone());
                serialize(
                    &json!({"notifications":notices,"next_notifications_after":next,
                    "delivery":"Durable references (including acknowledged notices), not real-user instructions. Continue until an empty page."}),
                )
            }
            Err(e) => failure(e.into()),
        };
    }
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
    let _authority = match state
        .task
        .config
        .ownership
        .as_ref()
        .map(|p| p.mutation_guard())
        .transpose()
    {
        Ok(guard) => guard,
        Err(e) => return failure(LaneError::new("ownership_lost", e)),
    };
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
        artifact_revision: args.artifact_revision,
        // Stable across failed reports from this execution; never model-supplied.
        failure_episode: (args.status == LaneStatus::Failed)
            .then(|| format!("agent:{}", state.caller_run_id())),
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
    match store.handle.append_lane_report_with_delivery(
        lane.lane_id,
        lane.management_revision,
        report,
        crate::run_store::review_time_ns(),
    ) {
        Ok(appended) => {
            drop(_authority);
            runtime
                .shared
                .bus
                .emit(Event::new(appended.lane.report_appended_event(
                    &appended.report,
                    Some(state.caller_run_id().to_string()),
                )));
            // Persistence succeeded even if guard reflection fails; never retry the write.
            let progress_error = LaneQuestionBindingSource::record_lane_question_report_progress(
                runtime,
                &appended.report,
            )
            .err();
            if let Some(lead) = &appended.lane.lead_thread_id {
                LaneQuestionBindingSource::schedule_lane_question_delivery(runtime, lead);
            }
            if let Some(error) = progress_error {
                serialize(
                    &json!({"report":appended.report,"saved":true,"progress_pending":error.to_string()}),
                )
            } else {
                serialize(&appended.report)
            }
        }
        Err(e) => failure(e.into()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuestionsArgs {
    #[serde(default)]
    cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnswerArgs {
    question_id: String,
    management_revision: u64,
    routing_revision: u64,
    action: AnswerAction,
    operation_id: String,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AnswerAction {
    Answer { text: String },
    RouteToUser,
}

pub(super) fn questions(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    let args: QuestionsArgs = match parse(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    let cursor = match args.cursor {
        Some(cursor) if cursor.len() <= 2048 => match serde_json::from_str(&cursor) {
            Ok(cursor) => Some(cursor),
            Err(_) => return failure(LaneError::new("stale_cursor", "invalid question cursor")),
        },
        Some(_) => {
            return failure(LaneError::new(
                "stale_cursor",
                "question cursor is too large",
            ));
        }
        None => None,
    };
    match runtime.list_lane_questions(state.caller_run_id(), cursor) {
        Ok(page) => serialize(&json!({"questions":page.questions,"has_more":page.has_more,
            "next_cursor":page.next_cursor.map(|c| serde_json::to_string(&c).expect("serializable cursor"))})),
        Err(e) => failure(e),
    }
}
pub(super) fn answer(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    let args: AnswerArgs = match parse(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    let answer = match args.action {
        AnswerAction::Answer { text } => Some(text),
        AnswerAction::RouteToUser => None,
    };
    match runtime.resolve_lane_question(
        state.caller_run_id(),
        LaneAnswerRequest {
            question_id: args.question_id,
            management_revision: args.management_revision,
            routing_revision: args.routing_revision,
            operation_id: args.operation_id,
            answer,
        },
    ) {
        Ok(question) => serialize(
            &json!({"status":if question.answer.is_some() { "answer_saved" } else { "routed_to_user" }, "question":question,
            "delivery":"Saved durably. Live recipients observe answers at a turn boundary; offline continuation is queued, not guaranteed to be running."}),
        ),
        Err(e) => failure(e),
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
        "Open an independent Orchestrator lane from an agent-authored brief; does not transfer this lead's goal or ownership. Workspace declarations grant no permissions. Returns lane/thread/root IDs. Attached root questions go to the lead only when durable delivery is bound; otherwise questions go to the user. Use lane_list for reports; ready remains unverified.",
        json!({"brief":{"type":"object","properties":{
        "objective":{"type":"string"},"inputs":{"type":"array","items":{"type":"object","properties":{"kind":{"enum":["description","thread","message","artifact"]},"value":{"type":"string"}},"required":["kind","value"],"additionalProperties":false}},
        "deliverables":strings(),"acceptance_criteria":strings(),"constraints":strings(),
        "workspace":{"type":"object","properties":{"branch":{"type":"string"},"worktree":{"type":"string"},"notes":{"type":"string"}},"additionalProperties":false},
        "verification_requirements":{"type":"object","properties":{"pull_request":{"type":"boolean","default":false}},"additionalProperties":false},
        "report_policy":{"enum":["attached","summary","detached"]}},"required":["objective","inputs","deliverables","acceptance_criteria","constraints","report_policy"],"additionalProperties":false}}),
        &["brief"],
    )
}
pub(super) fn list_spec(name: &str) -> providers::ToolSpec {
    object(
        name,
        "Read currently managed lanes in latest real-activity order (lane ID breaks ties), at most 20 entries/16 KiB. Continue with next_cursor and the same filter; stale_cursor requires a fresh listing. Reports are bounded self-reports, not verified completion. Pass notifications_after (empty string starts) to read durable notification references instead of lanes, at most 20/16 KiB including acknowledged notices; follow next_notifications_after until empty. Do not combine notification mode with lane cursor/filter. Reads do not update activity or wake anyone.",
        json!({"notifications_after":{"type":"string","maxLength":256},"filter":{"enum":["attached","summary"]},"cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20}}),
        &[],
    )
}
pub(super) fn report_spec(name: &str) -> providers::ToolSpec {
    object(
        name,
        "Append this lane root's bounded structured report (summary <=1 KiB, whole report <=4 KiB, at most 8 artifact refs). ready is an unverified claim, not completion/close. Important reports enter the durable delivery outbox; delivery requires a bound host. artifact_revision is an immutable commit SHA or SHA-256; failed episodes are assigned by runtime, not caller. Detached lanes do not report to a lead.",
        json!({"status":{"enum":["working","waiting","blocked","ready","failed"]},"summary":{"type":"string"},"artifact_refs":strings(),"artifact_revision":{"type":"object","properties":{"kind":{"enum":["commit_sha","sha256"]},"value":{"type":"string"}},"required":["kind","value"],"additionalProperties":false},"needs_human":{"type":"boolean"},"question_refs":strings(),"reason_code":{"type":"string"}}),
        &["status", "summary"],
    )
}

pub(super) fn questions_spec(name: &str) -> providers::ToolSpec {
    object(
        name,
        "List unanswered lead-addressed questions from currently managed lanes, at most 20 entries/16 KiB. Independent of this run's user_answers and finish gate. Continue with next_cursor; stale_cursor requires a fresh listing. Each page rechecks management. Do not poll.",
        json!({"cursor":{"type":"string","maxLength":2048}}),
        &[],
    )
}
pub(super) fn answer_spec(name: &str) -> providers::ToolSpec {
    object(
        name,
        "Resolve a current managed lane question using listed management/routing revisions. action.kind=answer answers as this lane lead (never real-user permission); route_to_user keeps the original question ID/requester. operation_id (1-256 bytes) identifies an identical retry. Saves atomically; stale_management, stale_routing, not_current_lead and already_answered reject competing operations. Answer delivery is durable and may remain pending for offline or stopped lanes.",
        json!({"question_id":{"type":"string"},"management_revision":{"type":"integer","minimum":0},"routing_revision":{"type":"integer","minimum":0},
            "operation_id":{"type":"string","minLength":1,"maxLength":256},
            "action":{"oneOf":[{"type":"object","properties":{"kind":{"const":"answer"},"text":{"type":"string","minLength":1,"maxLength":4096}},"required":["kind","text"],"additionalProperties":false},
                {"type":"object","properties":{"kind":{"const":"route_to_user"}},"required":["kind"],"additionalProperties":false}]}}),
        &[
            "question_id",
            "management_revision",
            "routing_revision",
            "action",
            "operation_id",
        ],
    )
}
