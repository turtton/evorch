use super::WorkbenchState;
use crate::model::agent_features::{AGENT_FEATURES, AgentFeaturesSettings};
use crate::model::tasks::AgentRunSource;
use crate::theme::{
    text::{h3, muted},
    tokens::*,
    widgets::{primary_button, surface_frame},
};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn open_agent_features(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        match config::Config::load(&self.user_settings_options()) {
            Ok(config) => self.agent_features = AgentFeaturesSettings::seed_from_config(&config),
            Err(error) => self.agent_features.error = Some(error.to_string()),
        }
        self.provider_settings.open = false;
        self.routing_settings.open = false;
        self.role_settings.open = false;
        self.sandbox_settings.open = false;
        self.self_improvement_settings.open = false;
        self.storage_settings.open = false;
        self.close_theme_settings();
        self.agent_features.open = true;
    }

    pub(super) fn render_agent_features(&mut self, ctx: &egui::Context) {
        if !self.agent_features.open {
            return;
        }
        let mut save = false;
        let mut cancel = false;
        let busy = self.settings_save_in_progress();
        egui::Modal::new(egui::Id::new("agent-features-settings"))
            .backdrop_color(palette().OVERLAY)
            .frame(surface_frame(palette().SURFACE_RAISED))
            .show(ctx, |ui| {
                ui.set_width((ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0);
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
                ui.label(h3("Agent features"));
                ui.label(muted("Turn agent tool features on or off. Saved to your user config."));
                ui.label(muted("A run's tool list is fixed when it starts: changes apply to the next run you start."));
                egui::ScrollArea::vertical().max_height(ctx.viewport_rect().height() * 0.6).show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        for (index, feature) in AGENT_FEATURES.iter().enumerate() {
                            if index > 0 {
                                ui.separator();
                            }
                            let default = if feature.default_on() { "on" } else { "off" };
                            ui.checkbox((feature.switch)(&mut self.agent_features.draft), feature.label);
                            ui.label(muted(feature.description));
                            ui.label(muted(format!("Default: {default}. {}", feature.applies)));
                        }
                    });
                });
                if let Some(error) = &self.agent_features.error {
                    crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
                }
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        save = primary_button(ui, "Save agent features").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            });
        if cancel {
            self.agent_features.open = false;
        }
        if save {
            self.save_agent_features();
        }
    }

    fn save_agent_features(&mut self) {
        let features = self.agent_features.draft;
        let result = self
            .provider_settings_path
            .as_ref()
            .ok_or_else(|| "No user config path is configured".to_owned())
            .and_then(|path| {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                config::LayeredSaveContext::new(&self.user_settings_options())
                    .save_agent_features(path, features)
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = result {
            self.agent_features.error = Some(error);
            return;
        }
        self.agent_features.saved = features;
        self.sandbox_settings.config.web_tools_enabled = features.web_tools_enabled;
        self.sink.submit(
            crate::model::commands::WorkbenchCommand::SetWebToolsEnabled {
                enabled: features.web_tools_enabled,
            },
        );
        if let Some(runtime) = &self.sandbox_settings.runtime {
            runtime.set_web_tools_enabled(features.web_tools_enabled);
            // The other run_js limits keep their startup values. Runs already
            // started keep the tool specs they sent (prompt cache contract).
            runtime.set_run_js_settings(config::RunJsConfig {
                enabled: features.run_js_enabled,
                ..runtime.run_js_settings()
            });
        }
        self.agent_features.error = None;
        self.agent_features.open = false;
        self.push_notice("Agent features saved — they apply to runs started from now");
    }
}
