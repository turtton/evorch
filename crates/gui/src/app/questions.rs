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

/// Pending questions the runtime routed to `lead_thread` rather than to the user. They are
/// answered by the lead (or passed to the user) through its lane tools, so the question list
/// never shows them; this count is how the lead's view tells you they exist.
pub(super) fn lead_routed_count<'a>(
    questions: impl IntoIterator<Item = &'a UserQuestion>,
    lead_thread: &str,
) -> usize {
    questions
        .into_iter()
        .filter(|question| {
            question.answer.is_none()
                && matches!(
                    &question.responder,
                    Some(event_bus::lane::QuestionResponder::Lead { thread_id })
                        if thread_id == lead_thread
                )
        })
        .count()
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
    fn a_question_routed_to_the_lead_is_hidden_from_the_user_and_counted_for_the_lead() {
        let mut routed = question("lane-q", "lane-root");
        routed.responder = Some(event_bus::lane::QuestionResponder::Lead {
            thread_id: "lead".into(),
        });
        assert!(!user_visible(&routed));
        assert!(!belongs_to_thread(&routed, "lane", &["lane-root".into()]));
        let other = {
            let mut other = routed.clone();
            other.id = "other".into();
            other.responder = Some(event_bus::lane::QuestionResponder::Lead {
                thread_id: "another-lead".into(),
            });
            other
        };
        let user = {
            let mut user = question("user-q", "lane-root");
            user.responder = Some(event_bus::lane::QuestionResponder::User);
            user
        };
        let all = [routed.clone(), other, user.clone()];
        assert_eq!(lead_routed_count(&all, "lead"), 1);
        assert_eq!(lead_routed_count(&all, "another-lead"), 1);
        assert_eq!(lead_routed_count(&all, "nobody"), 0);

        // Passing it to the user (the same question, answered by routing) makes it visible there.
        routed.responder = Some(event_bus::lane::QuestionResponder::User);
        assert!(user_visible(&routed));
        assert!(belongs_to_thread(&routed, "lane", &["lane-root".into()]));
        assert!(user_visible(&user));
        assert_eq!(lead_routed_count([&routed], "lead"), 0);
    }

    #[test]
    fn an_answered_lead_question_is_no_longer_waiting() {
        let mut routed = question("lane-q", "lane-root");
        routed.responder = Some(event_bus::lane::QuestionResponder::Lead {
            thread_id: "lead".into(),
        });
        routed.answer = Some("decided".into());
        assert_eq!(lead_routed_count([&routed], "lead"), 0);
    }

    fn question(id: &str, root: &str) -> UserQuestion {
        UserQuestion {
            responder: None,
            routing_revision: 0,
            management_revision: None,
            id: id.into(),
            run_id: root.into(),
            root_run_id: root.into(),
            root_name: "chat:Orchestrator:lane".into(),
            recipient_run_ids: Vec::new(),
            title: "scope".into(),
            options: vec![],
            blocking: true,
            answer: None,
            answer_origin: None,
        }
    }

    #[test]
    fn durable_chat_identity_recovers_a_question_without_a_run_start_event() {
        let question = UserQuestion {
            responder: None,
            routing_revision: 0,
            management_revision: None,
            id: "question-1".into(),
            run_id: "run-1".into(),
            root_run_id: "run-1".into(),
            root_name: "chat:Worker:one".into(),
            recipient_run_ids: Vec::new(),
            title: "scope".into(),
            options: vec![],
            blocking: true,
            answer: None,
            answer_origin: None,
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
