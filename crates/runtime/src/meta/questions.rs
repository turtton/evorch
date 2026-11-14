use super::{DispatchResult, EmptyArgs, error, parse, serialize};
use crate::{AgentRuntime, agent_loop::LoopState};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardArgs {
    question_id: String,
}

pub(super) fn forward_spec(name: &str) -> providers::ToolSpec {
    providers::ToolSpec {
        name: name.into(),
        description: "Forward an unanswered direct child's question from this current attached lane root to its lead. Keeps the original ID, requester and inherited recipients; never grants user permission. Caller, lane, revisions and retry identity are runtime-owned. Use only when this lane cannot resolve the question. No new question or capacity slot is created.".into(),
        input_schema: serde_json::json!({"type":"object","properties":{"question_id":{"type":"string","minLength":1,"maxLength":256}},"required":["question_id"],"additionalProperties":false}),
    }
}
pub(super) fn forward(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args: ForwardArgs = match parse(input) {
        Ok(args) => args,
        Err(e) => return error(e),
    };
    let Some(operation_id) = state.meta_call_id.as_deref() else {
        return error(
            serde_json::json!({"code":"invalid_input","message":"missing tool call identity"})
                .to_string(),
        );
    };
    match runtime.forward_lane_question(state.caller_run_id(), &args.question_id, operation_id) {
        Ok(route) => serialize(
            &serde_json::json!({"question_id":route.question_id,"responder":route.responder,"routing_revision":route.routing_revision}),
        ),
        Err(e) => error(serde_json::json!({"code":e.code,"message":e.message}).to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubagentQuestionsArgs {
    run_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AnswerSubagentQuestionArgs {
    question_id: String,
    answer: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AskArgs {
    title: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default = "blocking_default")]
    blocking: bool,
}
fn blocking_default() -> bool {
    true
}
pub(super) fn ask_user(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<AskArgs>(input) {
        Ok(a) => a,
        Err(e) => return error(e),
    };
    match runtime.request_user_question_outcome(
        state.caller_run_id(),
        args.title,
        args.options,
        args.blocking,
    ) {
        Ok(storage::lane_delivery::LaneQuestionCreated::Created(q)) => {
            let delivery = if matches!(
                q.responder,
                Some(event_bus::lane::QuestionResponder::Lead { .. })
            ) {
                "Your lane lead will answer or route this question to the user. Continue independent work; do not poll."
            } else if q.run_id == q.root_run_id {
                "The user can answer at a turn boundary. Continue independent work; do not poll."
            } else {
                "Your Orchestrator parent will answer or ask the user. Continue independent work; do not poll."
            };
            serialize(
                &serde_json::json!({"question_id":q.id,"status":"pending","blocking":q.blocking,"delivery":delivery}),
            )
        }
        Ok(storage::lane_delivery::LaneQuestionCreated::CapacityBlocked(diagnostic)) => {
            serialize(&serde_json::json!({
                "status":"question_capacity_blocked", "capacity":diagnostic.capacity,
                "limit":diagnostic.capacity.limit(), "retry_automatically":false,
                "delivery":"No question was created. Capacity is blocked; do not automatically retry."
            }))
        }
        Err(e) => error(e),
    }
}
pub(super) fn user_answers(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    if let Err(e) = parse::<EmptyArgs>(input) {
        return error(e);
    }
    match runtime.user_answers(state.caller_run_id()) {
        Ok(q) => serialize(&listing_window(q)),
        Err(e) => error(e),
    }
}

/// Most recent answered questions listed beside every unanswered one. Older
/// answers were already injected into the run (or, for a child's questions,
/// given by the reader itself); runtime logic reads the complete history, only
/// the model-facing listings of `user_answers` and `subagent_questions` are
/// bounded.
const ANSWERED_LISTING_LIMIT: usize = 32;

fn listing_window(questions: Vec<event_bus::UserQuestion>) -> Vec<event_bus::UserQuestion> {
    let answered = questions.iter().filter(|q| q.answer.is_some()).count();
    let mut skip = answered.saturating_sub(ANSWERED_LISTING_LIMIT);
    questions
        .into_iter()
        .filter(|question| {
            if question.answer.is_some() && skip > 0 {
                skip -= 1;
                return false;
            }
            true
        })
        .collect()
}

pub(super) fn subagent_questions(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<SubagentQuestionsArgs>(input) {
        Ok(args) => args,
        Err(e) => return error(e),
    };
    let child = match super::parse_run_id(&args.run_id) {
        Ok(child) => child,
        Err(e) => return error(e),
    };
    match runtime.subagent_questions(state.caller_run_id(), child) {
        Ok(questions) => {
            runtime.observe_questions(questions.iter().map(|question| question.id.clone()));
            // Listing a question reports it, so a later wait does not wake for it.
            state.reported_questions.extend(
                questions
                    .iter()
                    .filter(|question| question.answer.is_none())
                    .map(|question| question.id.clone()),
            );
            serialize(&listing_window(questions))
        }
        Err(e) => error(e),
    }
}

pub(super) async fn answer_subagent_question(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<AnswerSubagentQuestionArgs>(input) {
        Ok(args) => args,
        Err(e) => return error(e),
    };
    match runtime
        .answer_subagent_question(state.caller_run_id(), &args.question_id, &args.answer)
        .await
    {
        Ok(question) => serialize(&question),
        Err(e) => error(e),
    }
}
