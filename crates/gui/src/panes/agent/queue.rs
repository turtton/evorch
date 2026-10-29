//! Follow-ups waiting for a busy run, and messages that never reached the
//! agent, listed above goal and procedures.
//!
//! Rows read top to bottom in delivery order. Only the next one to send
//! carries Send sooner; each queued input can independently be edited/deleted.
use crate::model::composer::QueuedFollowUp;
use crate::panes::composer::ComposerAction;
use crate::theme::widgets::{accessible, ghost, icon_button, icon_text, labeled};
use crate::theme::{icons, tokens::*};

/// Rows beyond this scroll so a long queue keeps the input visible.
const VISIBLE_ROWS: f32 = 3.0;

#[derive(Clone, Copy)]
pub struct QueueBand<'a> {
    pub items: &'a [QueuedFollowUp],
    pub next_turn_requested: bool,
}

pub(super) fn rows(ui: &mut egui::Ui, band: QueueBand<'_>) -> Option<ComposerAction> {
    let mut action = None;
    let next = band.items.iter().position(|item| !item.undelivered);
    egui::ScrollArea::vertical()
        .id_salt("queued-follow-ups")
        .max_height(VISIBLE_ROWS * ROW_DENSE)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (index, item) in band.items.iter().enumerate() {
                ui.push_id(("queued-follow-up", index), |ui| {
                    if let Some(pressed) = row(ui, item, index, (next == Some(index), band)) {
                        action = Some(pressed);
                    }
                });
            }
        });
    action
}

/// Icon button whose tooltip names the action and whose accessible name also
/// names the message, so each row's control stays distinguishable.
fn item_button(ui: &mut egui::Ui, icon: &str, action: &str, text: &str) -> egui::Response {
    let side = ROW_DENSE - SP_1;
    let response = ui.add(ghost(icon_text(icon)).min_size(egui::vec2(side, side)));
    accessible(&response, &format!("{action}: {text}"));
    response.on_hover_text(action)
}

fn row(
    ui: &mut egui::Ui,
    item: &QueuedFollowUp,
    index: usize,
    (is_next, band): (bool, QueueBand<'_>),
) -> Option<ComposerAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        if item.undelivered {
            let response =
                ui.label(egui::RichText::new(icons::WARNING_CIRCLE).color(palette().WARNING_FG));
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "Not delivered")
            });
            response.on_hover_text(match &item.rejection {
                Some(reason) => reason.clone(),
                None => "Not delivered — the run ended first".into(),
            });
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if item.undelivered {
                if item_button(ui, icons::X, "Dismiss", &item.text).clicked() {
                    action = Some(ComposerAction::DismissQueued(index));
                }
                if item_button(ui, icons::PENCIL_SIMPLE, "Edit in composer", &item.text).clicked() {
                    action = Some(ComposerAction::RestoreQueued(index));
                }
            } else {
                if item_button(ui, icons::X, "Delete", &item.text).clicked() {
                    action = Some(ComposerAction::DeleteQueued(index));
                }
                if item_button(ui, icons::PENCIL_SIMPLE, "Edit", &item.text).clicked() {
                    action = Some(ComposerAction::EditQueued(index));
                }
                if is_next {
                    if band.next_turn_requested {
                        let response = ui.label(
                            egui::RichText::new(icons::HOURGLASS_MEDIUM)
                                .color(palette().TEXT_MUTED),
                        );
                        labeled(response, "Sending after the current tools");
                    } else if icon_button(ui, icons::KEY_RETURN, "Send sooner").clicked() {
                        action = Some(ComposerAction::DeliverNextTurn);
                    }
                }
            }
            let color = if is_next {
                palette().TEXT
            } else {
                palette().TEXT_MUTED
            };
            let line = item.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let response = ui
                .with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.set_min_height(ROW_DENSE - SP_1);
                    ui.add(egui::Label::new(egui::RichText::new(line).color(color)).truncate())
                })
                .inner;
            let state = if item.undelivered {
                "Not delivered"
            } else {
                "Queued"
            };
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    format!("{state}: {}", item.text),
                )
            });
            response.on_hover_text(&item.text);
        });
    });
    if item.undelivered {
        ui.label(
            egui::RichText::new(
                item.rejection
                    .as_deref()
                    .unwrap_or("Not delivered — the run ended first"),
            )
            .color(palette().WARNING_FG),
        );
    }
    action
}
