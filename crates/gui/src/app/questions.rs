use super::WorkbenchState;
use crate::model::{commands::WorkbenchCommand, tasks::AgentRunSource};
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
    pub(super) fn render_user_questions(&mut self, ctx: &egui::Context) {
        let Some(thread) = self.sidebar.active_thread.as_ref() else {
            return;
        };
        let thread_id = thread.to_string();
        let roots = self
            .sidebar
            .threads
            .iter()
            .find(|t| &t.id == thread)
            .map(|t| t.run_ids.clone())
            .unwrap_or_default();
        let questions: Vec<_> = self
            .user_questions
            .values()
            .filter(|q| user_visible(q) && belongs_to_thread(q, &thread_id, &roots))
            .cloned()
            .collect();
        if questions.is_empty() {
            return;
        }
        let mut reply = None;
        egui::Window::new("回答待ちの質問")
            .id(egui::Id::new("user-questions"))
            .resizable(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(350.0)
                    .show(ui, |ui| {
                        for q in questions {
                            ui.push_id(&q.id, |ui| {
                                ui.label(&q.title);
                                ui.small(format!(
                                    "{} · {}",
                                    q.run_id,
                                    if q.blocking {
                                        "回答に依存する作業は待機します"
                                    } else {
                                        "任意の回答"
                                    }
                                ));
                                let draft = self.question_drafts.entry(q.id.clone()).or_default();
                                for option in &q.options {
                                    if ui.selectable_label(draft == option, option).clicked() {
                                        *draft = option.clone();
                                    }
                                }
                                ui.add(
                                    egui::TextEdit::multiline(draft)
                                        .hint_text("回答を入力（選択肢以外でも可）")
                                        .desired_rows(2),
                                );
                                if ui
                                    .add_enabled(
                                        !draft.trim().is_empty() && draft.len() <= 4096,
                                        egui::Button::new("回答を送信"),
                                    )
                                    .clicked()
                                {
                                    reply = Some((q.id.clone(), draft.clone()));
                                }
                                ui.separator();
                            });
                        }
                    });
            });
        if let Some((question_id, answer)) = reply {
            self.submit_command(WorkbenchCommand::AnswerUserQuestion {
                thread_id,
                question_id,
                answer,
            });
        }
    }
}

// Child questions are addressed to their orchestrator. Only root questions,
// including explicit ask_user requests, are addressed to the person at the UI.
fn user_visible(question: &UserQuestion) -> bool {
    question.run_id == question.root_run_id
}

// The durable question remains discoverable even if its run-start event was
// not persisted. Chat identity follows the same exact role/thread namespace
// enforced by RuntimeCommandSink; arbitrary root names do not grant access.
fn belongs_to_thread(question: &UserQuestion, thread_id: &str, roots: &[String]) -> bool {
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
