//! The Config file tab (formerly Settings): the saved settings file as text, and whether what
//! it says is what the running core uses.
//!
//! Providers, Routing and Agent Roles are edited in Model Settings; this is for everything
//! else and for changing the file directly. Nothing here asks the core for anything while it
//! draws: the snapshot and the operations are read by `maintain`.
use super::*;
use crate::core_client::features::layout::main_paged_list;
use crate::theme::widgets::icon_button;

/// The button that asks the core to make the saved models, routes and roles the running ones.
pub(super) fn reload_models_button(ui: &mut egui::Ui, busy: bool) -> bool {
    let response = ui.add_enabled(
        !busy,
        egui::Button::new(icons::with_icon(icons::ARROWS_CLOCKWISE, "Reload models")),
    );
    accessible(&response, "Reload models");
    response.clicked()
}

/// The saved settings against the running core: a quiet line while they are the same, a banner
/// with the way to apply them while they are not.
fn applied_state(ui: &mut egui::Ui, snapshot: &SettingsSnapshot, busy: bool) -> bool {
    let applied = snapshot.applied_revision.is_some() && !snapshot.pending_apply();
    let mut reload = false;
    if applied {
        ui.horizontal_wrapped(|ui| {
            ui.label(muted(applied_label(snapshot)));
            reload = reload_models_button(ui, busy);
        });
        return reload;
    }
    soft_frame(palette().WARNING_SURFACE).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(icons::INFO).color(palette().WARNING_FG));
            ui.colored_label(palette().WARNING_FG, applied_label(snapshot));
            reload = reload_models_button(ui, busy);
        });
    });
    reload
}

impl SettingsFeature {
    pub(super) fn config_file(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        // Model Settings, open over this tab, carries the status and the applied state.
        if self.modal.is_some() {
            return;
        }
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        let Some(snapshot) = self.snapshot.clone() else {
            ui.label("Loading settings…");
            return;
        };
        if let Some(problem) = snapshot.problem {
            ui.colored_label(ui.visuals().warn_fg_color, problem_label(problem));
        }
        let busy = self.running();
        if applied_state(ui, &snapshot, busy) {
            self.reload_models(ctx);
        }
        if busy {
            ui.label("Working…");
        }
        ui.add_space(SP_1);
        let (mut reread, mut discard, mut save) = (false, false, false);
        let dirty = self.draft.as_ref().is_some_and(Draft::dirty);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("config.toml")
                    .monospace()
                    .color(palette().TEXT_MUTED),
            );
            if dirty {
                ui.label(egui::RichText::new("● edited").color(palette().WARNING_FG));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(dirty && !busy, |ui| {
                    let button = primary_button(ui, icons::with_icon(icons::FLOPPY_DISK, "Save"));
                    accessible(&button, "Save file");
                    save = button.clicked();
                });
                ui.add_enabled_ui(dirty, |ui| {
                    discard = icon_button(ui, icons::ARROW_COUNTER_CLOCKWISE, "Discard changes")
                        .clicked();
                });
                ui.add_enabled_ui(!busy, |ui| {
                    reread = icon_button(ui, icons::ARROWS_CLOCKWISE, "Re-read file").clicked();
                });
            });
        });
        if let Some(draft) = &mut self.draft {
            main_paged_list(ui, "settings-document", None, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut draft.text)
                        .code_editor()
                        .desired_width(f32::INFINITY),
                );
            });
        }
        ui.label(muted(
            icons::with_icon(
                icons::STACK_SIMPLE,
                "Providers, Routing and Agent Roles are edited in \u{2699} \u{203a} Model Settings. This tab is for everything else, and for changing the file directly.",
            ),
        ));
        if reread {
            self.draft = None;
            self.stale = true;
            let operation_id = new_operation();
            self.send(
                ctx,
                SettingsCommand::RefreshSettings(RefreshSettings { operation_id }),
            );
        }
        if discard {
            self.draft = None;
            self.stale = true;
        }
        if save && let Some(draft) = &self.draft {
            let (text, expected_revision) = (draft.text.clone(), draft.based_on.clone());
            let id = self.send_document(ctx, expected_revision, text.clone());
            self.saving.insert(id, text);
        }
    }
}
