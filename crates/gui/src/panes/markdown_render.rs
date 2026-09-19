//! Markdown presentation for transcript text.

use egui::Ui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::theme::tokens::palette;

pub fn render_markdown(ui: &mut Ui, source: &str, id_salt: &str) {
    ui.push_id(id_salt, |ui| {
        let color = ui.visuals().override_text_color.unwrap_or(palette().TEXT);
        ui.visuals_mut().widgets.noninteractive.fg_stroke.color = color;
        ui.visuals_mut().widgets.active.fg_stroke.color = color;
        ui.visuals_mut().extreme_bg_color = palette().SURFACE_RAISED;
        CommonMarkViewer::new().show(ui, &mut CommonMarkCache::default(), source);
    });
}

#[cfg(test)]
mod tests;
