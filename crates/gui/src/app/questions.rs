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
    question.is_user_visible()
}

// The durable question remains discoverable even if its run-start event was
// not persisted. Chat identity follows the same exact role/thread namespace
// enforced by RuntimeCommandSink; arbitrary root names do not grant access.
pub(super) fn belongs_to_thread(
    question: &UserQuestion,
    thread_id: &str,
    roots: &[String],
) -> bool {
    question.belongs_to_thread(thread_id, roots)
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
            recipient_run_ids: Vec::new(),
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
        goal.recipient_run_ids = vec!["run-2".into(), "run-3".into()];
        assert!(belongs_to_thread(&goal, "recipient", &["run-3".into()]));
        assert!(!belongs_to_thread(&goal, "unrelated", &["run-4".into()]));
        goal.run_id = "child".into();
        assert!(!user_visible(&goal));
        assert!(!belongs_to_thread(&goal, "one", &["run-1".into()]));
        assert!(!belongs_to_thread(&goal, "recipient", &["run-3".into()]));
    }

    #[test]
    fn legacy_question_events_default_to_no_inherited_recipients() {
        let question: UserQuestion = serde_json::from_value(serde_json::json!({
            "id":"q", "run_id":"run-1", "root_run_id":"run-1",
            "root_name":"chat:Worker:one", "title":"scope", "options":[],
            "blocking":true, "answer":null
        }))
        .unwrap();
        assert!(question.recipient_run_ids.is_empty());
        assert!(belongs_to_thread(&question, "one", &[]));
    }
}
