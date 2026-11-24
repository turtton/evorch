//! The theme settings modal, shared by the embedded workbench and the core client.
//!
//! Pure drawing: the caller passes the preset in effect and the last save error, and
//! gets back what was pressed. Installing the preset and saving it stay with the owner.
use crate::theme::style::ThemePreset;
use crate::theme::tokens::{PROVIDER_MODAL_MAX_WIDTH, SP_2, SP_4, palette};
use crate::theme::widgets::surface_frame;

pub const PRESETS: [(&str, &str, ThemePreset); 3] = [
    ("graphite", "Graphite", ThemePreset::Graphite),
    ("tokyo-night", "Tokyo Night", ThemePreset::TokyoNight),
    ("high-contrast", "High Contrast", ThemePreset::HighContrast),
];

pub const NO_SETTINGS_PATH: &str = "No UI settings path is configured; theme is not saved";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThemeModal {
    pub selected: Option<ThemePreset>,
    pub close: bool,
}

pub fn theme_settings_modal(
    ctx: &egui::Context,
    current: ThemePreset,
    error: Option<&str>,
) -> ThemeModal {
    let mut selected = None;
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::new("theme-settings"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(crate::theme::text::h3("Theme settings"));
            for (id, label, preset) in PRESETS {
                ui.push_id(id, |ui| {
                    if ui.radio(current == preset, label).clicked() {
                        selected = Some(preset);
                    }
                });
            }
            if let Some(error) = error {
                crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
            }
            close = ui.button("Close").clicked();
        });
    ThemeModal {
        selected,
        close: close || modal.should_close(),
    }
}
