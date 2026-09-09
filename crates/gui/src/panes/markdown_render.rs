//! Markdown presentation for agent responses only.

use egui::Ui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::theme::tokens::{ACCENT_FG, SURFACE_RAISED, TEXT};

pub fn render_markdown(ui: &mut Ui, source: &str, id_salt: &str) {
    ui.push_id(id_salt, |ui| {
        ui.visuals_mut().override_text_color = None;
        ui.visuals_mut().widgets.noninteractive.fg_stroke.color = TEXT;
        ui.visuals_mut().widgets.active.fg_stroke.color = ACCENT_FG;
        ui.visuals_mut().extreme_bg_color = SURFACE_RAISED;
        CommonMarkViewer::new().show(ui, &mut CommonMarkCache::default(), source);
    });
}

#[cfg(test)]
mod tests;
