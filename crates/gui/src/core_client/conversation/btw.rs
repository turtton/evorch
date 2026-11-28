//! The side question's card: an inline frame directly above the composer, never a popup.
use std::path::Path;

use egui::{RichText, Ui};

use crate::core_client::features::execution::{BtwCardView, CardState};
use crate::panes::markdown_render::render_markdown_in_workspace;
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::{icon_button, soft_frame, spinner};

const ASKING: &str = "Asking from the conversation so far…";
const UNSETTLED: &str = "Answered from the conversation as of its latest request; a reply still being written is not in it.";
const TRUNCATED: &str = "The answer was cut at the core's size limit.";

/// What pressing the card asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BtwChoice {
    Close,
}

/// Draws the card, at most `max_height` tall: a long answer scrolls inside it, so the
/// composer below keeps its room.
pub(super) fn btw_card(
    ui: &mut Ui,
    card: &BtwCardView<'_>,
    base: Option<&Path>,
    max_height: f32,
) -> Option<BtwChoice> {
    let mut choice = None;
    soft_frame(palette().SURFACE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("/btw").strong());
            ui.add(
                egui::Label::new(muted(card.question.lines().next().unwrap_or_default()))
                    .truncate(),
            )
            .on_hover_text(card.question);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, icons::X, "Close side question").clicked() {
                    choice = Some(BtwChoice::Close);
                }
            });
        });
        let body_height = (max_height - ROW_DENSE).max(ROW_DENSE);
        match card.state {
            CardState::Asking => {
                ui.horizontal(|ui| {
                    spinner(ui);
                    ui.label(muted(ASKING));
                });
            }
            CardState::Answered {
                text,
                truncated,
                source,
            } => {
                egui::ScrollArea::vertical()
                    .id_salt(("btw-answer", card.operation.as_str()))
                    .max_height(body_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        render_markdown_in_workspace(
                            ui,
                            text,
                            &format!("btw-{}", card.operation.as_str()),
                            base,
                        );
                    });
                if source.as_ref().is_some_and(|source| !source.settled) {
                    ui.label(muted(UNSETTLED));
                }
                if *truncated {
                    ui.label(muted(TRUNCATED));
                }
            }
            CardState::Failed(reason) => {
                egui::ScrollArea::vertical()
                    .id_salt(("btw-failure", card.operation.as_str()))
                    .max_height(body_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.colored_label(palette().ERROR_FG, reason);
                    });
            }
        }
    });
    choice
}
