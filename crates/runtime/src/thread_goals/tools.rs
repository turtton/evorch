use super::*;
use crate::agent_loop::LoopState;
use crate::meta::{DispatchResult, Terminal};
use providers::ToolSpec;
use serde_json::{Value, json};

pub(crate) fn spec(name: &str) -> ToolSpec {
    let (description, properties, required) = match name {
        "create_goal" => (
            "Proactively track an objective from the user's request when it benefits from sustained work or verification. Any kind of requested work can be a goal, including research and local edits. Never add scope or permissions. One unfinished objective per thread. Completion requires an end-of-turn self-check; optional independent review is controlled by the user. Only the trusted thread root may create a goal.",
            json!({"objective":{"type":"string"},"criteria":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":32}}),
            vec!["objective", "criteria"],
        ),
        "get_goal" => (
            "Inspect the current thread objective, acceptance criteria, epoch, evidence, review findings and cumulative usage. Only the trusted thread root may access this state.",
            json!({}),
            vec![],
        ),
        "submit_goal_check" => (
            "Submit evidence for every current acceptance criterion after the runtime requests an end-of-turn self-check. Use the exact current epoch from get_goal. A check without evidence cannot complete a goal. Report unmet or unknown criteria as met=false and continue the original work. This tool adds no authority to execute actions.",
            json!({"epoch":{"type":"integer","minimum":0},"checks":checks_schema()}),
            vec!["epoch", "checks"],
        ),
        "submit_goal_review" => (
            "Submit your independent review against the original user request, current acceptance criteria and actual outputs. Report defects, unmet requirements or insufficient evidence in findings; do not require optional improvements or expand scope. Every criterion needs concrete evidence. Only the runtime-assigned reviewer may submit for its exact epoch. Submission ends the review.",
            json!({"epoch":{"type":"integer","minimum":0},"checks":checks_schema(),"findings":{"type":"array","items":{"type":"string"},"maxItems":32}}),
            vec!["epoch", "checks", "findings"],
        ),
        _ => unreachable!(),
    };
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    }
}
fn checks_schema() -> Value {
    json!({"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","properties":{"criterion":{"type":"integer","minimum":0},"met":{"type":"boolean"},"evidence":{"type":"string"}},"required":["criterion","met","evidence"],"additionalProperties":false}})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    objective: String,
    criteria: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    epoch: u64,
    checks: Vec<ThreadGoalCheck>,
}

pub(crate) fn dispatch(
    state: &LoopState,
    runtime: &AgentRuntime,
    name: &str,
    input: Value,
) -> DispatchResult {
    let result = handle(state, runtime, name, input);
    match result {
        Ok(value) if name == "submit_goal_review" => DispatchResult {
            result: ::tools::ToolResult::success(value.to_string()),
            terminal: Terminal::Finish("Independent goal review submitted.".into()),
        },
        Ok(value) => crate::meta::success(value.to_string()),
        Err(reason) => crate::meta::error(reason),
    }
}
fn handle(
    state: &LoopState,
    runtime: &AgentRuntime,
    name: &str,
    input: Value,
) -> Result<Value, String> {
    let root = state.caller_run_id();
    if name == "submit_goal_review" {
        let crate::RunPurpose::ThreadGoalReview { root_run_id, epoch } = state.run_config().purpose
        else {
            return Err("only the assigned independent reviewer may submit".into());
        };
        let review: GoalReview = crate::meta::parse(input)?;
        let mut goals = runtime.goal_lock();
        let thread = goals
            .roots
            .get(&root_run_id)
            .cloned()
            .ok_or("review root no longer bound")?;
        let entry = goals
            .goals
            .get_mut(&thread)
            .ok_or("goal no longer exists")?;
        if entry.reviewer != Some(root)
            || epoch != review.epoch
            || epoch != entry.snapshot.epoch
            || entry.snapshot.checks_paused
            || entry.snapshot.work_stopped
            || entry.snapshot.phase != ThreadGoalPhase::Reviewing
        {
            return Err("stale or unauthorized goal review".into());
        }
        validate_checks(&entry.snapshot.criteria, &review.checks, true)?;
        validate_findings(&review.findings)?;
        entry.review_result = Some(review);
        return Ok(json!({"submitted":true}));
    }
    if state.task.parent.is_some() || state.run_config().purpose != crate::RunPurpose::General {
        return Err("only the trusted thread root can manage its goal".into());
    }
    let thread = runtime
        .goal_thread(root)
        .ok_or("this run is not a registered thread root")?;
    match name {
        "create_goal" => {
            let args: Create = crate::meta::parse(input)?;
            runtime.create_agent_thread_goal(&thread, root, args.objective, args.criteria)?;
            let mut goals = runtime.goal_lock();
            let original_request = goals
                .requests
                .get(&root)
                .cloned()
                .unwrap_or_else(|| bounded(&state.task.prompt, MAX_TEXT));
            let entry = goals.goals.get_mut(&thread).ok_or("goal disappeared")?;
            entry.snapshot.original_request = original_request;
            runtime.publish_thread_goal(&entry.snapshot);
            Ok(json!(entry.snapshot))
        }
        "get_goal" => {
            let _: crate::meta::EmptyArgs = crate::meta::parse(input)?;
            Ok(json!(runtime.thread_goal(&thread)))
        }
        "submit_goal_check" => {
            let args: Check = crate::meta::parse(input)?;
            let mut goals = runtime.goal_lock();
            let entry = goals.goals.get_mut(&thread).ok_or("no current goal")?;
            if args.epoch != entry.snapshot.epoch
                || entry.snapshot.phase != ThreadGoalPhase::Checking
                || entry.snapshot.checks_paused
                || entry.snapshot.work_stopped
            {
                return Err("self-check is stale or was not requested; inspect get_goal".into());
            }
            validate_checks(&entry.snapshot.criteria, &args.checks, true)?;
            let passed = args.checks.iter().all(|check| check.met);
            entry.snapshot.checks = args.checks;
            if !passed {
                entry.snapshot.phase = ThreadGoalPhase::Repairing;
                entry.snapshot.epoch = entry.snapshot.epoch.saturating_add(1);
            }
            runtime.publish_thread_goal(&entry.snapshot);
            Ok(
                json!({"accepted":true,"all_criteria_met":passed,"next":"Finish this turn if all criteria are met; otherwise address the unmet criteria within the original request."}),
            )
        }
        _ => Err("unknown goal operation".into()),
    }
}
