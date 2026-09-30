use super::WorkbenchState;
use crate::model::{
    self_improvement_settings::SelfImprovementSettingsModel, tasks::AgentRunSource,
};
use crate::theme::{
    text::{h3, muted},
    tokens::*,
    widgets::{primary_button, surface_frame},
};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn open_self_improvement_settings(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        match config::Config::load(&self.routing_load_options()) {
            Ok(config) => {
                self.self_improvement_settings =
                    SelfImprovementSettingsModel::seed_from_config(&config.self_improvement);
            }
            Err(error) => self.self_improvement_settings.error = Some(error.to_string()),
        }
        self.provider_settings.open = false;
        self.routing_settings.open = false;
        self.role_settings.open = false;
        self.sandbox_settings.open = false;
        self.close_theme_settings();
        self.self_improvement_settings.open = true;
    }

    pub(super) fn poll_self_improvement_save(&mut self) {
        if self.self_improvement_settings.poll_save() {
            self.push_notice("Self-improvement settings saved — restart evorch to apply");
        }
    }

    pub(super) fn render_self_improvement_settings(&mut self, ctx: &egui::Context) {
        if !self.self_improvement_settings.open {
            return;
        }
        let mut save = false;
        let mut cancel = false;
        let busy = self.settings_save_in_progress();
        egui::Modal::new(egui::Id::new("self-improvement-settings"))
            .backdrop_color(palette().OVERLAY)
            .frame(surface_frame(palette().SURFACE_RAISED))
            .show(ctx, |ui| {
                ui.set_width((ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0);
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
                ui.label(h3("Self-improvement drafts"));
                ui.label(muted("Drafts only — evorch never files issues automatically (ADR 0011)."));
                ui.label(muted("Restart evorch after saving to apply changes. Collection in this session is unchanged."));
                ui.label(muted(if self.self_improvement.enabled {
                    "Current session: enabled"
                } else {
                    "Current session: disabled (default off)"
                }));
                egui::ScrollArea::vertical().max_height(ctx.viewport_rect().height() * 0.5).show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        settings_fields(ui, &mut self.self_improvement_settings);
                    });
                });
                if let Some(error) = &self.self_improvement_settings.error {
                    ui.colored_label(palette().ERROR_FG, error);
                }
                if busy {
                    ui.label(muted("Saving settings…"));
                }
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        save = primary_button(ui, "Save self-improvement").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            });
        if cancel {
            self.self_improvement_settings.open = false;
        }
        if save {
            self.self_improvement_settings
                .start_save(self.provider_settings_path.clone());
        }
    }
}

fn settings_fields(ui: &mut egui::Ui, model: &mut SelfImprovementSettingsModel) {
    let defaults = config::SelfImprovementConfig::default();
    ui.checkbox(
        &mut model.draft.enabled,
        "Enable self-improvement drafts (default off)",
    );
    ui.checkbox(&mut model.draft.collect_diagnostics, "Collect diagnostics");
    ui.checkbox(&mut model.draft.collect_lessons, "Collect lessons");
    ui.label("Draft directory");
    ui.add(
        egui::TextEdit::singleline(&mut model.draft_dir)
            .hint_text("default: <storage>/self-improvement/drafts")
            .desired_width(f32::INFINITY),
    );
    egui::Grid::new("self_improvement_limits")
        .num_columns(3)
        .show(ui, |ui| {
            for (label, value, default, range) in [
                (
                    "Max candidates",
                    &mut model.draft.max_candidates,
                    defaults.max_candidates,
                    1..=10_000,
                ),
                (
                    "Evidence max bytes",
                    &mut model.draft.evidence_max_bytes,
                    defaults.evidence_max_bytes,
                    256..=65_536,
                ),
                (
                    "Daily limit",
                    &mut model.draft.daily_limit,
                    defaults.daily_limit,
                    1..=1_000,
                ),
            ] {
                ui.label(label);
                ui.add(egui::DragValue::new(value).range(range));
                ui.label(muted(format!("default: {default}")));
                ui.end_row();
            }
            ui.label("Duplicate cooldown (seconds)");
            ui.add(
                egui::DragValue::new(&mut model.draft.duplicate_cooldown_secs)
                    .range(60..=31_536_000),
            );
            ui.label(muted(format!(
                "default: {}",
                defaults.duplicate_cooldown_secs
            )));
            ui.end_row();
        });
}
