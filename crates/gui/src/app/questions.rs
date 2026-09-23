use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use event_bus::{Event, EventKind, ToolEvent, UserQuestion};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn pending_user_questions(&self) -> impl Iterator<Item = &UserQuestion> {
        self.user_questions.values()
    }
    pub(super) fn fold_user_question(&mut self, event: &Event) {
        if let EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) = &event.kind {
            if question.answer.is_some() {
                self.user_questions.remove(&question.id);
                self.question_drafts.remove(&question.id);
            } else {
                self.user_questions
                    .insert(question.id.clone(), question.clone());
            }
        }
    }
}

// Child questions are addressed to their orchestrator. Only root questions,
// including explicit ask_user requests, are addressed to the person at the UI.
pub(super) fn user_visible(question: &UserQuestion) -> bool {
    question.run_id == question.root_run_id
}

// The durable question remains discoverable even if its run-start event was
// not persisted. Chat identity follows the same exact role/thread namespace
// enforced by RuntimeCommandSink; arbitrary root names do not grant access.
pub(super) fn belongs_to_thread(
    question: &UserQuestion,
    thread_id: &str,
    roots: &[String],
) -> bool {
    roots.contains(&question.root_run_id)
        || [runtime::Role::Worker, runtime::Role::Orchestrator]
            .into_iter()
            .any(|role| question.root_name == format!("chat:{}:{thread_id}", role.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_chat_identity_recovers_a_question_without_a_run_start_event() {
        let question = UserQuestion {
            id: "question-1".into(),
            run_id: "run-1".into(),
            root_run_id: "run-1".into(),
            root_name: "chat:Worker:one".into(),
            title: "scope".into(),
            options: vec![],
            blocking: true,
            answer: None,
        };
        assert!(belongs_to_thread(&question, "one", &[]));
        assert!(!belongs_to_thread(&question, "two", &[]));
        assert!(!belongs_to_thread(&question, "one:extra", &[]));
        let mut goal = question;
        goal.root_name = "goal-1".into();
        assert!(!belongs_to_thread(&goal, "one", &[]));
        assert!(belongs_to_thread(&goal, "one", &["run-1".into()]));
    }
}
