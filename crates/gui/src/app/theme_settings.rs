use std::path::PathBuf;

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use crate::theme::style::ThemePreset;
use crate::theme::tokens::{PROVIDER_MODAL_MAX_WIDTH, SP_2, SP_4, palette};
use crate::theme::widgets::surface_frame;

pub(super) struct ThemeSettings {
    pub open: bool,
    settings: workspace_ui::UiSettings,
    path: Option<PathBuf>,
    error: Option<String>,
}

impl ThemeSettings {
    pub const fn new(settings: workspace_ui::UiSettings) -> Self {
        Self {
            open: false,
            settings,
            path: None,
            error: None,
        }
    }
}

impl From<workspace_ui::ThemePresetName> for ThemePreset {
    fn from(value: workspace_ui::ThemePresetName) -> Self {
        match value {
            workspace_ui::ThemePresetName::Graphite => Self::Graphite,
            workspace_ui::ThemePresetName::TokyoNight => Self::TokyoNight,
            workspace_ui::ThemePresetName::HighContrast => Self::HighContrast,
        }
    }
}

impl From<ThemePreset> for workspace_ui::ThemePresetName {
    fn from(value: ThemePreset) -> Self {
        match value {
            ThemePreset::Graphite => Self::Graphite,
            ThemePreset::TokyoNight => Self::TokyoNight,
            ThemePreset::HighContrast => Self::HighContrast,
        }
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub const fn theme_preset(&self) -> ThemePreset {
        self.theme_preset
    }

    pub fn with_ui_settings_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.theme_settings.path = Some(path.into());
        self
    }

    pub fn open_theme_settings(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        self.provider_settings.open = false;
        self.role_settings.open = false;
        self.routing_settings.open = false;
        self.theme_settings.open = true;
    }

    pub fn close_theme_settings(&mut self) {
        self.theme_settings.open = false;
    }

    pub(super) fn render_theme_settings(&mut self, ctx: &egui::Context) {
        if !self.theme_settings.open {
            return;
        }
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
                for (id, label, preset) in [
                    ("graphite", "Graphite", ThemePreset::Graphite),
                    ("tokyo-night", "Tokyo Night", ThemePreset::TokyoNight),
                    ("high-contrast", "High Contrast", ThemePreset::HighContrast),
                ] {
                    ui.push_id(id, |ui| {
                        if ui.radio(self.theme_preset == preset, label).clicked() {
                            selected = Some(preset);
                        }
                    });
                }
                if let Some(error) = &self.theme_settings.error {
                    ui.colored_label(palette().ERROR_FG, error);
                }
                close = ui.button("Close").clicked();
            });
        if let Some(preset) = selected {
            self.reload_theme(ctx, preset);
            self.theme_settings.settings.theme_preset = preset.into();
            self.theme_settings.error = match &self.theme_settings.path {
                Some(path) => workspace_ui::save_settings(&self.theme_settings.settings, path)
                    .err()
                    .map(|error| {
                        tracing::error!(%error, "UI settings save failed");
                        error.to_string()
                    }),
                None => Some("No UI settings path is configured; theme is not saved".into()),
            };
        }
        if close || modal.should_close() {
            self.close_theme_settings();
        }
    }
}
