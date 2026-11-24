use std::path::PathBuf;

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use crate::panes::theme_widgets::{NO_SETTINGS_PATH, ThemeModal, theme_settings_modal};
use crate::theme::style::ThemePreset;

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

impl<S: AgentRunSource> WorkbenchState<S> {
    pub const fn theme_preset(&self) -> ThemePreset {
        self.theme_preset
    }

    pub fn with_ui_settings_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.theme_settings.path = Some(path.into());
        self
    }

    /// Writes the UI settings file with the current sidebar view. `None` means no
    /// settings path is configured; `Some(error)` is the failure of the write.
    fn save_ui_settings(&mut self) -> Option<Option<String>> {
        self.theme_settings.settings.sidebar_view = self.sidebar_view.clone();
        let path = self.theme_settings.path.as_ref()?;
        Some(
            workspace_ui::save_settings(&self.theme_settings.settings, path)
                .err()
                .map(|error| {
                    tracing::error!(%error, "UI settings save failed");
                    error.to_string()
                }),
        )
    }

    /// Persists a sort or fold change to the UI settings file, which every normal
    /// start reads back. Removed projects are dropped from the saved view.
    pub(super) fn sidebar_view_changed(&mut self, changed: bool) {
        if changed {
            self.sidebar_view.retain_projects(&self.sidebar);
            self.save_ui_settings();
        }
    }

    pub fn open_theme_settings(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        self.dismiss_settings_modal();
        self.sandbox_settings.open = false;
        self.agent_features.open = false;
        self.self_improvement_settings.open = false;
        self.theme_settings.open = true;
    }

    pub fn close_theme_settings(&mut self) {
        self.theme_settings.open = false;
    }

    pub(super) fn render_theme_settings(&mut self, ctx: &egui::Context) {
        if !self.theme_settings.open {
            return;
        }
        let ThemeModal { selected, close } =
            theme_settings_modal(ctx, self.theme_preset, self.theme_settings.error.as_deref());
        if let Some(preset) = selected {
            self.reload_theme(ctx, preset);
            self.theme_settings.settings.theme_preset = preset.into();
            self.theme_settings.error = self
                .save_ui_settings()
                .unwrap_or_else(|| Some(NO_SETTINGS_PATH.into()));
        }
        if close {
            self.close_theme_settings();
        }
    }
}
