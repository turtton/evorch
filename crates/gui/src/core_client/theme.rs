//! The theme picker under ⚙ → Theme: the embedded workbench's modal, a live preview, and
//! the chosen preset written back to `ui.toml`.
use std::path::{Path, PathBuf};

use crate::panes::theme_widgets::{NO_SETTINGS_PATH, ThemeModal, theme_settings_modal};
use crate::theme::style::ThemePreset;

pub struct ThemePicker {
    pub open: bool,
    preset: ThemePreset,
    path: Option<PathBuf>,
    error: Option<String>,
}

impl Default for ThemePicker {
    fn default() -> Self {
        Self::new(None, ThemePreset::Graphite)
    }
}

impl ThemePicker {
    pub fn new(path: Option<PathBuf>, preset: ThemePreset) -> Self {
        Self {
            open: false,
            preset,
            path,
            error: None,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let ThemeModal { selected, close } =
            theme_settings_modal(ctx, self.preset, self.error.as_deref());
        if let Some(preset) = selected {
            crate::theme::style::install_preset(ctx, preset);
            self.preset = preset;
            self.error = match &self.path {
                None => Some(NO_SETTINGS_PATH.into()),
                Some(path) => save_theme_preset(path, preset).err(),
            };
        }
        if close {
            self.open = false;
        }
    }
}

/// Changes only the preset in `path`, keeping what the embedded workbench wrote there (its
/// layout, keybinds and sidebar view). A file that exists but does not load is left as it
/// is: it may be a newer version's, and overwriting it would lose the rest.
pub fn save_theme_preset(path: &Path, preset: ThemePreset) -> Result<(), String> {
    let mut settings = if path.exists() {
        workspace_ui::load_settings(path).map_err(|error| error.to_string())?
    } else {
        workspace_ui::UiSettings::default()
    };
    settings.theme_preset = preset.into();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    workspace_ui::save_settings(&settings, path).map_err(|error| {
        tracing::error!(%error, "UI settings save failed");
        error.to_string()
    })
}
