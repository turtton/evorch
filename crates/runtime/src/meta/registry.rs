//! Runtime-owned meta operations, ordered as they appear in provider tool specs.

use providers::ToolSpec;
use serde_json::Value;

use super::{
    DispatchResult, compaction, delegation, error, escalation, finish, ledger, messaging, parse,
    questions, runs, skills, specs, success,
};
use crate::{AgentRuntime, agent_loop::LoopState};

/// Define each operation's public name, model-facing spec and callable handler.
/// A new entry must provide both function paths before the crate compiles.
macro_rules! define_meta_ops {
    ($( $variant:ident => $name:literal => $spec:path => |$state:ident, $runtime:ident, $input:ident| $handler:block ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum MetaOp {
            $( $variant, )+
        }

        impl MetaOp {
            pub(crate) fn from_name(name: &str) -> Option<Self> {
                match name {
                    $( $name => Some(Self::$variant), )+
                    _ => None,
                }
            }

            pub(crate) fn spec(self) -> ToolSpec {
                match self {
                    $( Self::$variant => $spec($name), )+
                }
            }

            #[allow(unused_variables)]
            pub(crate) async fn handle(
                self,
                state: &mut LoopState,
                runtime: &AgentRuntime,
                input: Value,
            ) -> DispatchResult {
                match self {
                    $( Self::$variant => {
                        let $state = state;
                        let $runtime = runtime;
                        let $input = input;
                        $handler
                    }, )+
                }
            }
        }

        pub const META_OPS: &[&str] = &[ $( $name, )+ ];
    };
}

define_meta_ops! {
    Delegate => "delegate" => specs::delegate => |state, runtime, input| {
        delegation::delegate(state, runtime, input).await
    },
    SendMessage => "send_message" => specs::send_message => |state, runtime, input| {
        messaging::send_message(state, runtime, input)
    },
    SkillLoad => "skill_load" => specs::skill_load => |state, runtime, input| {
        skills::skill_load(state, input)
    },
    Wait => "wait" => specs::wait => |state, runtime, input| {
        runs::wait(state, runtime, input).await
    },
    RunOutput => "run_output" => specs::run_output => |state, runtime, input| {
        runs::run_output(state, runtime, input)
    },
    Cancel => "cancel" => specs::cancel => |state, runtime, input| {
        runs::cancel(runtime, input)
    },
    ListAgents => "list_agents" => specs::list_agents => |state, runtime, input| {
        runs::list_agents(runtime, input)
    },
    InspectAgent => "inspect_agent" => specs::inspect_agent => |state, runtime, input| {
        runs::inspect_agent(runtime, input)
    },
    Compact => "compact" => specs::compact => |state, runtime, input| {
        compaction::compact(state, input).await
    },
    Finish => "finish" => specs::finish => |state, runtime, input| {
        finish(state, runtime, input).await
    },
    Send => "send" => specs::send => |state, runtime, input| {
        messaging::send(state, runtime, input)
    },
    WaitReply => "wait_reply" => specs::wait_reply => |state, runtime, input| {
        messaging::wait_reply(state, runtime, input).await
    },
    Inbox => "inbox" => specs::inbox => |state, runtime, input| {
        messaging::inbox(state, runtime, input)
    },
    Escalate => "escalate" => specs::escalate => |state, runtime, input| {
        escalation::escalate(state, runtime, input).await
    },
    LedgerAppend => "ledger_append" => specs::ledger_append => |state, runtime, input| {
        ledger::append(state, runtime, input)
    },
    LedgerRead => "ledger_read" => specs::ledger_read => |state, runtime, input| {
        ledger::read(state, runtime, input)
    },
    SubmitReview => "submit_review" => specs::submit_review => |state, runtime, input| {
        match parse(input) {
            Ok(result) => {
                runtime.submit_review(state.caller_run_id(), result);
                success("review submitted")
            }
            Err(message) => error(message),
        }
    },
    AskUser => "ask_user" => specs::ask_user => |state, runtime, input| {
        questions::ask_user(state, runtime, input)
    },
    UserAnswers => "user_answers" => specs::user_answers => |state, runtime, input| {
        questions::user_answers(state, runtime, input)
    },
    SubagentQuestions => "subagent_questions" => specs::subagent_questions => |state, runtime, input| {
        questions::subagent_questions(state, runtime, input)
    },
    AnswerSubagentQuestion => "answer_subagent_question" => specs::answer_subagent_question => |state, runtime, input| {
        questions::answer_subagent_question(state, runtime, input)
    },
    InspectLearningSource => "inspect_learning_source" => specs::inspect_learning_source => |state, runtime, input| {
        learning_result(parse(input).and_then(|args| {
            runtime.inspect_learning_source(state.caller_run_id(), state.run_config(), args)
        }))
    },
    StackLessonCandidate => "stack_lesson_candidate" => specs::stack_lesson_candidate => |state, runtime, input| {
        learning_result(parse(input).and_then(|args| {
            runtime.stack_lesson_candidate(state.caller_run_id(), state.run_config(), args)
        }))
    },
    ListLessonCandidates => "list_lesson_candidates" => specs::list_lesson_candidates => |state, runtime, input| {
        learning_result(parse(input).and_then(|args| {
            runtime.list_lesson_candidates(state.caller_run_id(), state.run_config(), args)
        }))
    },
    SubmitLessonReview => "submit_lesson_review" => specs::submit_lesson_review => |state, runtime, input| {
        learning_result(parse(input).and_then(|args| {
            runtime.submit_lesson_review(state.caller_run_id(), state.run_config(), args)
        }))
    },
}

fn learning_result(result: Result<Value, String>) -> DispatchResult {
    match result {
        Ok(value) => success(value.to_string()),
        Err(reason) => error(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_names_are_unique_and_resolvable() {
        let mut names = std::collections::BTreeSet::new();
        for name in META_OPS {
            assert!(names.insert(*name), "duplicate meta operation: {name}");
            assert!(MetaOp::from_name(name).is_some());
        }
        assert_eq!(names.len(), META_OPS.len());
    }
}
