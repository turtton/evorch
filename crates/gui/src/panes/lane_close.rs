//! The confirmation for ending a lane by hand (ADR 0044 §9).
//!
//! It shows what the ledger says (the brief, the latest Ready, and the review, evidence and
//! pull request as reference only), what the operation means, and requires a reason. It
//! decides nothing: it returns Confirm, Reload or Cancel, and the app sends the revisions it
//! read to the trusted host path.

use egui::{Align, Layout};

use crate::app::lane_close::{CloseDialog, ClosePhase};
use crate::model::lane_close::{self, CloseChoice, MEANING};
use crate::theme::text::{h3, medium, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{ghost, primary_button, surface_frame};

pub const NOTE_LABEL: &str = "Reason (required)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDialogAction {
    Confirm,
    Reload,
    Cancel,
}

pub fn lane_close_dialog(
    ctx: &egui::Context,
    dialog: &mut CloseDialog,
    running: bool,
) -> Option<CloseDialogAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("lane-close-dialog"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3(dialog.choice.title()));
            ui.label(medium(&dialog.title));
            ui.add(egui::Label::new(muted(MEANING)).wrap());
            ui.add(egui::Label::new(muted(lane_close::REMAINS)).wrap());
            if running {
                warning(ui, lane_close::RUNNING_WARNING);
            }
            if let Some(blocker) = dialog.blocker() {
                let color = if matches!(dialog.phase, ClosePhase::Loading) {
                    palette().TEXT_MUTED
                } else {
                    palette().WARNING_FG
                };
                ui.add(egui::Label::new(egui::RichText::new(blocker).color(color)).wrap());
            }
            if let ClosePhase::Loaded(preview) = &dialog.phase {
                if preview.ready.is_none() && dialog.choice == CloseChoice::Complete {
                    warning(ui, lane_close::NO_READY_WARNING);
                }
                if let Some(line) = lane_close::open_questions_line(preview.open_question_count) {
                    warning(ui, &line);
                }
                egui::ScrollArea::vertical()
                    .id_salt("lane-close-body")
                    .max_height(260.0)
                    .show(ui, |ui| {
                        let brief = &preview.lane.brief;
                        ui.label(medium("Brief"));
                        ui.add(egui::Label::new(&brief.objective).wrap());
                        for criterion in &brief.acceptance_criteria {
                            ui.add(egui::Label::new(muted(format!("• {criterion}"))).wrap());
                        }
                        ui.separator();
                        match &preview.ready {
                            Some(ready) => {
                                ui.label(medium(format!("Latest Ready (#{})", ready.report_seq)));
                                ui.add(egui::Label::new(&ready.summary).wrap());
                                ui.label(muted(lane_close::evidence_line(ready)));
                                let artifacts = lane_close::artifact_lines(ready);
                                if !artifacts.is_empty() {
                                    egui::CollapsingHeader::new(format!(
                                        "Artifact references ({})",
                                        ready.artifact_refs.len()
                                            + usize::from(ready.artifact_revision.is_some())
                                    ))
                                    .id_salt("lane-close-artifacts")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for line in artifacts {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(line).monospace(),
                                                )
                                                .wrap(),
                                            );
                                        }
                                    });
                                }
                            }
                            None => {
                                ui.label(medium("No Ready report"));
                                ui.label(muted(
                                    "The lane has not reported Ready for its current brief.",
                                ));
                            }
                        }
                        if let Some(readiness) = preview.readiness {
                            ui.label(muted(lane_close::review_line(readiness)));
                        }
                        ui.label(muted(lane_close::pr_line(&preview.pr_context)));
                        ui.label(muted(
                            "Review, evidence and the pull request are reference only; none of them is required to proceed.",
                        ));
                    });
            }
            ui.separator();
            ui.label(muted(NOTE_LABEL));
            let edit = ui.add(
                egui::TextEdit::multiline(&mut dialog.note)
                    .desired_rows(3)
                    .hint_text("Why are you ending this lane?")
                    .desired_width(f32::INFINITY),
            );
            edit.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, NOTE_LABEL)
            });
            if let Some(error) = dialog.note_error().filter(|_| !dialog.note.is_empty()) {
                ui.colored_label(palette().ERROR_FG, error);
            }
            if let Some(notice) = &dialog.notice {
                ui.add(
                    egui::Label::new(egui::RichText::new(notice).color(palette().ERROR_FG)).wrap(),
                );
            }
            ui.label(muted(dialog.choice.recorded_as()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let confirm = ui
                    .add_enabled_ui(dialog.can_confirm(), |ui| {
                        primary_button(ui, dialog.choice.confirm())
                    })
                    .inner;
                if confirm.clicked() && dialog.can_confirm() {
                    action = Some(CloseDialogAction::Confirm);
                }
                if ui.add(ghost("Reload")).clicked() {
                    action = Some(CloseDialogAction::Reload);
                }
                if ui.add(ghost("Cancel")).clicked() {
                    action = Some(CloseDialogAction::Cancel);
                }
            });
        });
    if action.is_none() && modal.should_close() {
        action = Some(CloseDialogAction::Cancel);
    }
    action
}

fn warning(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).color(palette().WARNING_FG)).wrap());
}
