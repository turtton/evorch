use egui::{RichText, Ui};

use crate::model::{pending_approvals::PendingApprovalsModel, scoped_call::parse_scoped_call_id};
use crate::theme::tokens::{FONT_MONO, INFO, R_SM, SP_2, STATUS_STROKE, TEXT, TEXT_MUTED};

#[derive(Debug, PartialEq, Eq)]
pub enum ApprovalsAction {
    Decide { call_id: String, approved: bool },
}

pub fn approvals_pane(ui: &mut Ui, model: &PendingApprovalsModel) -> Option<ApprovalsAction> {
    let mut action = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        let mut items = model.items().peekable();
        if items.peek().is_none() {
            ui.label(RichText::new("保留中の承認要求はありません").color(TEXT_MUTED));
        }
        for item in items {
            ui.push_id(&item.call_id, |ui| {
                egui::Frame::new()
                    .stroke(egui::Stroke::new(STATUS_STROKE, INFO))
                    .corner_radius(R_SM)
                    .inner_margin(SP_2)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&item.tool_name).strong().color(TEXT))
                                .wrap(),
                        );
                        let original = parse_scoped_call_id(&item.call_id)
                            .map(|(_, call, _)| call)
                            .unwrap_or_else(|| item.call_id.clone());
                        let call = if original.is_empty() {
                            "—"
                        } else {
                            &original
                        };
                        let attempt = item.attempt.map_or_else(|| "—".into(), |n| n.to_string());
                        let correlation = format!(
                            "{} · {call} · attempt {attempt}",
                            item.run_id.as_deref().unwrap_or("—")
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(correlation)
                                    .monospace()
                                    .size(FONT_MONO)
                                    .color(TEXT_MUTED),
                            )
                            .wrap(),
                        )
                        .on_hover_text(&item.call_id);
                        let summary = match item.input.as_ref() {
                            Some(input) => {
                                let input = input.to_string();
                                let mut chars = input.chars();
                                let mut summary: String = chars.by_ref().take(120).collect();
                                if chars.next().is_some() {
                                    summary.push('…');
                                }
                                summary
                            }
                            None => "引数情報なし".into(),
                        };
                        ui.add(egui::Label::new(RichText::new(summary).color(TEXT_MUTED)).wrap());
                        ui.horizontal_wrapped(|ui| {
                            for (label, approved) in [("Approve", true), ("Reject", false)] {
                                if ui.button(label).clicked() {
                                    action = Some(ApprovalsAction::Decide {
                                        call_id: item.call_id.clone(),
                                        approved,
                                    });
                                }
                            }
                        });
                    });
                ui.add_space(SP_2);
            });
        }
    });
    action
}
