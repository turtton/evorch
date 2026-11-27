//! The screen of a conversation the core does not have yet: a heading over the composer,
//! which sits a little below the middle. The composer is the thread's own, so the first
//! message takes the same path as any other and the thread is created by that message.
use egui::{Align, Layout, Ui};
use workbench_protocol::ids::{ProjectId, ThreadId};

use super::{Activity, ConversationIo, ConversationPane, composer};
use crate::core_client::shell::Effect;
use crate::panes::transcript_widgets::event_line;
use crate::theme::icons;
use crate::theme::text::{h2, h3};
use crate::theme::tokens::*;

/// The composer is as wide as this, or as the pane when that is narrower.
const MAX_WIDTH: f32 = 720.0;
/// Where the composer's middle sits, as a share of the pane's height.
const COMPOSER_CENTER: f32 = 0.55;

/// What the last pass measured, which this pass places the composer by.
#[derive(Clone, Copy, Default)]
struct Measured {
    heading: f32,
    composer: f32,
}

pub(super) fn new_chat(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &mut ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    let measured_id = ui.id().with("new-chat-measured");
    let last = ui
        .data(|data| data.get_temp::<Measured>(measured_id))
        .unwrap_or_default();
    let pane_height = ui.available_height();
    let width = ui.available_width().min(MAX_WIDTH);
    let left = ui.max_rect().center().x - width / 2.0;
    let mut measured = last;
    egui::ScrollArea::vertical()
        .id_salt("new-chat")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // A short or crowded pane (several lines, files, completions) gives up the space
            // above the heading before anything becomes unreachable: it scrolls instead.
            let above = (pane_height * COMPOSER_CENTER - last.composer / 2.0 - last.heading - SP_4)
                .max(SP_4);
            ui.add_space(above);
            let rect = egui::Rect::from_min_size(
                egui::pos2(left, ui.cursor().top()),
                egui::vec2(width, pane_height.max(1.0)),
            );
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(Layout::top_down(Align::Min)),
                |ui| {
                    measured.heading = heading(ui, io.project_name);
                    ui.add_space(SP_4);
                    measured.composer = ui
                        .scope(|ui| {
                            composer::composer(
                                ui,
                                pane,
                                io,
                                project,
                                thread,
                                Activity::default(),
                                effects,
                            );
                        })
                        .response
                        .rect
                        .height();
                    // What /help and a usage error answer, which no transcript is here to show.
                    if let Some(notices) = pane.notices.get(thread.as_str()) {
                        for notice in notices {
                            event_line(ui, icons::INFO, notice, palette().TEXT_MUTED, None);
                        }
                    }
                },
            );
        });
    if (measured.heading - last.heading).abs() > 0.5
        || (measured.composer - last.composer).abs() > 0.5
    {
        ui.data_mut(|data| data.insert_temp(measured_id, measured));
        ui.ctx().request_discard("new conversation layout changed");
        ui.ctx().request_repaint();
    }
}

/// The software's name over what to do in the project; returns the height used.
fn heading(ui: &mut Ui, project_name: &str) -> f32 {
    ui.vertical_centered(|ui| {
        ui.add(egui::Label::new(h2("evorch")));
        ui.add(egui::Label::new(h3(format!("What should we do in {project_name}?"))).wrap());
    })
    .response
    .rect
    .height()
}
