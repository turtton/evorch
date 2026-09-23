use super::{DispatchResult, EmptyArgs, error, parse, serialize};
use crate::{AgentRuntime, agent_loop::LoopState};
use serde::Deserialize;

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
    match runtime.request_user_question(
        state.caller_run_id(),
        args.title,
        args.options,
        args.blocking,
    ) {
        Ok(q) => {
            let delivery = if q.run_id == q.root_run_id {
                "The user can answer at a turn boundary. Continue independent work; do not poll."
            } else {
                "Your Orchestrator parent will answer or ask the user. Continue independent work; do not poll."
            };
            serialize(
                &serde_json::json!({"question_id":q.id,"status":"pending","blocking":q.blocking,"delivery":delivery}),
            )
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
        Ok(q) => serialize(&q),
        Err(e) => error(e),
    }
}

pub(super) fn subagent_questions(
    state: &LoopState,
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
        Ok(questions) => serialize(&questions),
        Err(e) => error(e),
    }
}

pub(super) fn answer_subagent_question(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<AnswerSubagentQuestionArgs>(input) {
        Ok(args) => args,
        Err(e) => return error(e),
    };
    match runtime.answer_subagent_question(state.caller_run_id(), &args.question_id, &args.answer) {
        Ok(question) => serialize(&question),
        Err(e) => error(e),
    }
}
