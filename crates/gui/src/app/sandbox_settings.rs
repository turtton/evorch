use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use crate::theme::{
    text::{h3, muted},
    tokens::*,
    widgets::{primary_button, surface_frame},
};

#[derive(Default)]
pub(super) struct SandboxSettings {
    pub open: bool,
    pub config: config::SandboxConfig,
    error: Option<String>,
    runtime: Option<runtime::AgentRuntime>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn with_sandbox_runtime(mut self, runtime: runtime::AgentRuntime) -> Self {
        self.sandbox_settings.runtime = Some(runtime);
        self
    }

    pub fn open_sandbox_settings(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        match config::Config::load(&self.routing_load_options()) {
            Ok(config) => {
                self.sandbox_settings.config = config.sandbox;
                self.sandbox_settings.error = None;
            }
            Err(error) => self.sandbox_settings.error = Some(error.to_string()),
        }
        self.provider_settings.open = false;
        self.routing_settings.open = false;
        self.role_settings.open = false;
        self.close_theme_settings();
        self.sandbox_settings.open = true;
    }

    pub(super) fn render_sandbox_settings(&mut self, ctx: &egui::Context) {
        if !self.sandbox_settings.open {
            return;
        }
        let mut save = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("sandbox-settings"))
            .backdrop_color(palette().OVERLAY)
            .frame(surface_frame(palette().SURFACE_RAISED))
            .show(ctx, |ui| {
                ui.set_width((ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0);
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
                ui.label(h3("Sandbox"));
                ui.checkbox(&mut self.sandbox_settings.config.allow_network, "Allow network inside sandbox");
                ui.label(muted("Applies to new runs. Shares the host network without destination restrictions. Roles that deny network remain blocked. Web tool permissions are unchanged."));
                if let Some(error) = &self.sandbox_settings.error {
                    ui.colored_label(palette().ERROR_FG, error);
                }
                ui.horizontal(|ui| {
                    save = primary_button(ui, "Save sandbox").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if cancel {
            self.sandbox_settings.open = false;
        }
        if save {
            let result = self
                .provider_settings_path
                .as_ref()
                .ok_or_else(|| "No project config path is configured".to_owned())
                .and_then(|path| {
                    config::save_sandbox(path, self.sandbox_settings.config)
                        .map_err(|error| error.to_string())
                });
            match result {
                Ok(()) => {
                    if let Some(runtime) = &self.sandbox_settings.runtime {
                        runtime.set_sandbox_network(self.sandbox_settings.config.allow_network);
                    }
                    self.sandbox_settings.error = None;
                    self.push_notice("Sandbox settings updated");
                }
                Err(error) => self.sandbox_settings.error = Some(error),
            }
        }
    }
}
