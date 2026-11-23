//! Inline interaction cards shared by tool approvals and user questions.

use std::collections::BTreeMap;

use egui::{RichText, Ui};
use event_bus::UserQuestion;

use crate::model::pending_approvals::PendingApproval;
use crate::theme::tokens::palette;

const MAX_ANSWER_BYTES: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub enum RequestAction {
    Decide {
        call_id: String,
        approved: bool,
    },
    Answer {
        thread_id: String,
        question_id: String,
        answer: String,
    },
}

pub struct ConversationRequests<'a> {
    pub thread_id: &'a str,
    pub approvals: Vec<&'a PendingApproval>,
    pub questions: Vec<&'a UserQuestion>,
    pub drafts: &'a mut BTreeMap<String, String>,
}

impl ConversationRequests<'_> {
    pub fn is_empty(&self) -> bool {
        self.approvals.is_empty() && self.questions.is_empty()
    }

    pub fn show(&mut self, ui: &mut Ui) -> Option<RequestAction> {
        let mut action = None;
        for approval in &self.approvals {
            if let Some(decision) = super::approvals::approval_card(ui, approval) {
                action = Some(decision);
            }
        }
        for question in &self.questions {
            ui.push_id(("question", &question.id), |ui| {
                request_card(ui, "Question for you", |ui| {
                    ui.add(egui::Label::new(RichText::new(&question.title).strong()).wrap());
                    ui.add(
                        egui::Label::new(
                            RichText::new(if question.blocking {
                                "Work that depends on this answer is waiting"
                            } else {
                                "Optional answer"
                            })
                            .small()
                            .color(palette().TEXT_MUTED),
                        )
                        .wrap(),
                    );
                    let draft = self.drafts.entry(question.id.clone()).or_default();
                    for option in &question.options {
                        if ui
                            .add(egui::Button::selectable(draft == option, option).wrap())
                            .clicked()
                        {
                            *draft = option.clone();
                        }
                    }
                    ui.add(
                        egui::TextEdit::multiline(draft)
                            // Use the question's stable scope, not the changing transcript's auto ID.
                            .id_salt("answer-input")
                            .hint_text("Type an answer (free text or pick an option)")
                            .desired_width(f32::INFINITY)
                            .desired_rows(2),
                    );
                    if ui
                        .add_enabled(
                            !draft.trim().is_empty() && draft.len() <= MAX_ANSWER_BYTES,
                            egui::Button::new("Send answer"),
                        )
                        .on_disabled_hover_text(if draft.len() > MAX_ANSWER_BYTES {
                            "The answer is longer than 4096 bytes"
                        } else {
                            "Type or pick an answer first"
                        })
                        .clicked()
                    {
                        action = Some(RequestAction::Answer {
                            thread_id: self.thread_id.into(),
                            question_id: question.id.clone(),
                            answer: draft.clone(),
                        });
                    }
                });
            });
        }
        action
    }
}

pub(super) use crate::panes::transcript_widgets::request_card;
