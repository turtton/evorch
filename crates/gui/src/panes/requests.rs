//! Inline interaction cards shared by tool approvals and user questions.

use std::collections::BTreeMap;

use egui::{RichText, Ui};
use event_bus::UserQuestion;

use crate::model::pending_approvals::PendingApproval;
use crate::theme::tokens::{R_SM, SP_2, STATUS_STROKE, palette};

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
                request_card(ui, "回答待ちの質問", |ui| {
                    ui.add(egui::Label::new(RichText::new(&question.title).strong()).wrap());
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "{} · {}",
                                question.run_id,
                                if question.blocking {
                                    "回答に依存する作業は待機します"
                                } else {
                                    "任意の回答"
                                }
                            ))
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
                            .hint_text("回答を入力（選択肢以外でも可）")
                            .desired_width(f32::INFINITY)
                            .desired_rows(2),
                    );
                    if ui
                        .add_enabled(
                            !draft.trim().is_empty() && draft.len() <= 4096,
                            egui::Button::new("回答を送信"),
                        )
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

pub(super) fn request_card(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(palette().SURFACE)
        .stroke(egui::Stroke::new(STATUS_STROKE, palette().WARNING_FG))
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(RichText::new(title).strong().color(palette().WARNING_FG)).wrap(),
            );
            ui.add_space(SP_2);
            body(ui);
        });
    ui.add_space(SP_2);
}
