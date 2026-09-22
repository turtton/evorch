use event_bus::UserQuestion;
#[cfg(test)]
use providers::{ContentBlock, Role};

use super::LoopState;

/// Computed from one durable snapshot. In particular, an answer arriving after
/// the last model request is work to consume, not permission to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserQuestionReadiness {
    Ready,
    AnswersAvailable,
    Waiting,
}

impl LoopState {
    pub(crate) fn user_question_readiness(&self) -> Result<UserQuestionReadiness, String> {
        let Some(runtime) = self.runtime() else {
            return Ok(UserQuestionReadiness::Ready);
        };
        let questions = runtime.user_answers(self.task.run_id)?;
        if questions
            .iter()
            .any(|question| question.answer.is_some() && !self.has_user_answer(question))
        {
            return Ok(UserQuestionReadiness::AnswersAvailable);
        }
        if questions
            .iter()
            .any(|question| question.blocking && question.answer.is_none())
        {
            return Ok(UserQuestionReadiness::Waiting);
        }
        Ok(UserQuestionReadiness::Ready)
    }

    pub(crate) fn user_question_completion_check(&self) -> Result<(), String> {
        match self.user_question_readiness()? {
            UserQuestionReadiness::Ready => Ok(()),
            UserQuestionReadiness::AnswersAvailable => Err(
                "New user answers have arrived. Read and apply them before finishing.".into(),
            ),
            UserQuestionReadiness::Waiting => Err(
                "Required user answers are pending. Continue independent work; stop your turn to wait for the answer.".into(),
            ),
        }
    }

    pub(super) fn flush_user_answers(&mut self) -> Result<bool, String> {
        let Some(runtime) = self.runtime() else {
            return Ok(false);
        };
        let questions = runtime.user_answers(self.task.run_id)?;
        let mut received = false;
        for question in questions {
            if question.answer.is_none() || self.answered_questions.contains(&question.id) {
                continue;
            }
            // A restored run rebuilds its in-memory set from the persisted user
            // message. Reinjecting it on every restart duplicates instructions.
            let already_in_history = self.has_user_answer(&question);
            self.answered_questions.insert(question.id.clone());
            if !already_in_history {
                self.context.push_user(&answer_message(&question));
                received = true;
            }
        }
        if received {
            self.publish_message_count();
            self.resumed = true;
        }
        Ok(received)
    }

    fn has_user_answer(&self, question: &UserQuestion) -> bool {
        self.answered_questions.contains(&question.id)
            || answer_in_history(&self.context.messages, question)
    }
}

fn answer_message(question: &UserQuestion) -> String {
    format!(
        "[user-answer id={}]\nQuestion: {}\nAnswer: {}",
        question.id,
        question.title,
        question.answer.as_deref().unwrap_or_default(),
    )
}

fn answer_in_history(messages: &[providers::Message], question: &UserQuestion) -> bool {
    crate::AgentRuntime::answer_is_in_history(messages, question)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_answer_requires_matching_user_message_not_model_claim() {
        let question = UserQuestion {
            id: "question-1".into(),
            run_id: "run-1".into(),
            root_run_id: "run-1".into(),
            root_name: "chat:Worker:thread".into(),
            title: "scope?".into(),
            options: vec![],
            blocking: true,
            answer: Some("A".into()),
        };
        let mut message = providers::Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: answer_message(&question),
            }],
        };
        assert!(!answer_in_history(&[message.clone()], &question));
        message.role = Role::User;
        assert!(answer_in_history(&[message], &question));
    }
}
