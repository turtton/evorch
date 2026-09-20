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
    pub fn with_provider_settings_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.provider_settings_path = Some(path.into());
        self.load_sandbox_settings();
        self
    }

    fn load_sandbox_settings(&mut self) {
        match config::Config::load(&self.routing_load_options()) {
            Ok(config) => {
                self.sandbox_settings.config = config.sandbox;
                self.sandbox_settings.error = None;
            }
            Err(error) => self.sandbox_settings.error = Some(error.to_string()),
        }
    }

    pub(super) fn set_sandbox_escalation(&mut self, mode: config::EscalationApproval) {
        if self.settings_save_in_progress() {
            return;
        }
        let previous = self.sandbox_settings.config;
        self.sandbox_settings.config.escalation_approval = mode;
        self.save_sandbox_settings();
        if self.sandbox_settings.error.is_some() {
            self.sandbox_settings.config = previous;
            self.sandbox_settings.open = true;
        }
    }

    pub fn with_sandbox_runtime(mut self, runtime: runtime::AgentRuntime) -> Self {
        self.sandbox_settings.runtime = Some(runtime);
        self
    }

    pub fn open_sandbox_settings(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        self.load_sandbox_settings();
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
        let busy = self.settings_save_in_progress();
        egui::Modal::new(egui::Id::new("sandbox-settings"))
            .backdrop_color(palette().OVERLAY)
            .frame(surface_frame(palette().SURFACE_RAISED))
            .show(ctx, |ui| {
                ui.set_width((ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0);
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
                ui.label(h3("Sandbox"));
                ui.add_enabled_ui(!busy, |ui| {
                    ui.checkbox(&mut self.sandbox_settings.config.allow_network, "Allow network inside sandbox");
                    ui.label(muted("Applies to new runs. Shares the host network without destination restrictions. Roles that deny network remain blocked. Web tool permissions are unchanged."));
                });
                if let Some(error) = &self.sandbox_settings.error {
                    ui.colored_label(palette().ERROR_FG, error);
                }
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        save = primary_button(ui, "Save sandbox").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            });
        if cancel {
            self.load_sandbox_settings();
            self.sandbox_settings.open = false;
        }
        if save {
            self.save_sandbox_settings();
        }
    }

    fn save_sandbox_settings(&mut self) {
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
                    runtime.set_sandbox_escalation(
                        self.sandbox_settings.config.escalation_approval,
                        self.sandbox_settings.config.escalate_to_user_on_deny,
                    );
                }
                self.sandbox_settings.error = None;
                self.push_notice("Sandbox settings updated");
            }
            Err(error) => self.sandbox_settings.error = Some(error),
        }
    }
}
