//! Model Settings, drawn over the whole window: what goes into each region of the modal frame
//! (`panes::settings`) and what its controls ask of the feature.
//!
//! The modal is the feature's own overlay, not part of a tab: it is drawn and kept up to date
//! whichever tab is in front, and opening it neither creates nor selects the Config file tab.
//! Each section's body is one call below (`providers_body`, `Modal::body`), so a section that
//! moves into its own module changes that call and nothing else here.
use super::config_file::reload_models_button;
use super::*;
use crate::panes::model_settings::{ModalPart, SettingsModalAction, settings_modal};
use crate::theme::widgets::{ghost_icon_button, primary_icon_button};

/// What the footer's confirmation to close asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseAnswer {
    Discard,
    KeepEditing,
}

/// What the footer's Save and Discard asked for: the selected route or role.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FooterAction {
    Save,
    Discard,
}

/// The applied state of the saved settings, at the bottom of the navigation. Returns whether
/// the reload was asked for.
fn nav_footer(ui: &mut egui::Ui, snapshot: Option<&SettingsSnapshot>, busy: bool) -> bool {
    let Some(snapshot) = snapshot else {
        return false;
    };
    let applied = snapshot.applied_revision.is_some() && !snapshot.pending_apply();
    // Laid out bottom up: the button is the lowest line.
    let reload = !applied && reload_models_button(ui, busy);
    let (icon, color) = if applied {
        (icons::CHECK, palette().SUCCESS)
    } else {
        (icons::WARNING, palette().WARNING_FG)
    };
    ui.label(
        egui::RichText::new(icons::with_icon(icon, applied_label(snapshot)))
            .size(FONT_SMALL)
            .color(color),
    );
    reload
}

impl SettingsFeature {
    pub(super) fn render_model_settings(
        &mut self,
        egui_ctx: &egui::Context,
        ctx: &mut FeatureContext<'_>,
    ) {
        let Some(tab) = self.modal_tab() else {
            return;
        };
        let busy = self.running();
        let snapshot = self.snapshot.clone();
        let mut provider_action = None;
        let mut save: Option<ModalSave> = None;
        let mut reload = false;
        let mut answer = None;
        let mut footer_action = None;
        let action = settings_modal(egui_ctx, tab, busy, |ui, part| match part {
            ModalPart::HeaderRight => {
                if let Some(modal) = &mut self.modal {
                    save = modal.header_profile(ui, busy).or(save.take());
                }
            }
            ModalPart::NavFooter => reload = nav_footer(ui, snapshot.as_ref(), busy),
            ModalPart::Body { height } => match (tab, &snapshot) {
                (SettingsTab::Providers, Some(snapshot)) => {
                    provider_action = self.providers_body(ui, ctx, snapshot, busy, height);
                }
                (SettingsTab::Providers, None) => {
                    ui.label("Loading settings…");
                }
                _ => {
                    if let Some(modal) = &mut self.modal {
                        save = modal
                            .body(ui, snapshot.as_ref(), busy, height)
                            .or(save.take());
                    }
                }
            },
            ModalPart::Footer => {
                let (close, footer, provider) = self.footer(ui, busy);
                answer = close;
                footer_action = footer;
                provider_action = provider.or(provider_action.take());
            }
        });
        match (footer_action, &mut self.modal) {
            (Some(FooterAction::Save), Some(modal)) => save = modal.save_selected().or(save),
            (Some(FooterAction::Discard), Some(modal)) => modal.discard_selected(),
            _ => {}
        }
        if reload {
            self.reload_models(ctx);
        }
        match action {
            Some(SettingsModalAction::SelectTab(tab)) => {
                if let Some(modal) = &mut self.modal {
                    modal.select(tab);
                }
                if tab == SettingsTab::Providers {
                    self.auth.want_status();
                }
                self.confirm_close = false;
            }
            Some(SettingsModalAction::Close) => self.request_close(),
            None => {}
        }
        match answer {
            Some(CloseAnswer::Discard) => self.close_modal(),
            Some(CloseAnswer::KeepEditing) => self.confirm_close = false,
            None => {}
        }
        if let (Some(action), Some(snapshot)) = (provider_action, &snapshot) {
            self.apply_provider_action(ctx, snapshot, action);
            egui_ctx.request_repaint();
        }
        if let Some(save) = save {
            let id = self.send_document(ctx, save.expected_revision.clone(), save.text.clone());
            self.modal_saves.insert(id, save);
        }
    }

    /// The status of the last operation, or the question whether to discard what was edited;
    /// in Routing and Agent Roles also Discard and Save for the selected route or role, and in
    /// Providers those of the selected provider.
    fn footer(
        &self,
        ui: &mut egui::Ui,
        busy: bool,
    ) -> (
        Option<CloseAnswer>,
        Option<FooterAction>,
        Option<ProviderAction>,
    ) {
        if self.confirm_close {
            ui.colored_label(palette().WARNING_FG, "Discard unsaved changes and close?");
            let mut answer = None;
            ui.add_enabled_ui(!busy, |ui| {
                if ui.button("Discard and close").clicked() {
                    answer = Some(CloseAnswer::Discard);
                }
                if ui.button("Keep editing").clicked() {
                    answer = Some(CloseAnswer::KeepEditing);
                }
            });
            return (answer, None, None);
        }
        let section = self.modal.as_ref().and_then(Modal::footer_state);
        let providers = self.modal_tab() == Some(SettingsTab::Providers) && self.form.is_some();
        let summary = section.as_ref().and_then(|state| state.summary.clone());
        let state = if busy {
            Some(("Working…".to_owned(), palette().TEXT_MUTED))
        } else if let Some(summary) = summary {
            Some((format!("● {summary}"), palette().WARNING_FG))
        } else if self.modal_dirty() {
            let name = self
                .form
                .as_ref()
                .filter(|_| providers)
                .map(|form| form.original.clone().unwrap_or_else(|| form.name.clone()));
            Some((
                match name {
                    Some(name) => format!("● Unsaved changes in {name}"),
                    None => "● Unsaved changes".to_owned(),
                },
                palette().WARNING_FG,
            ))
        } else {
            None
        };
        let buttons = if section.is_some() {
            210.0
        } else if providers {
            190.0
        } else {
            0.0
        };
        let reserved = buttons + if state.is_some() { 190.0 } else { 0.0 };
        ui.scope(|ui| {
            ui.set_max_width((ui.available_width() - reserved).max(0.0));
            if !self.status.is_empty() {
                ui.add(egui::Label::new(self.status.as_str()).truncate())
                    .on_hover_text(&self.status);
            }
        });
        let mut chosen = None;
        let mut provider = None;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if providers {
                provider = self.provider_buttons(ui, busy);
            }
            if let Some(section) = &section {
                let save = ui
                    .add_enabled_ui(!busy && section.can_save, |ui| {
                        primary_icon_button(ui, icons::FLOPPY_DISK, section.save_label)
                    })
                    .inner;
                if let Some(why) = section.why_not {
                    save.clone().on_disabled_hover_text(why);
                }
                if save.clicked() {
                    chosen = Some(FooterAction::Save);
                }
                let discard = ui
                    .add_enabled_ui(!busy && section.can_discard, |ui| {
                        ghost_icon_button(ui, icons::ARROW_COUNTER_CLOCKWISE, "Discard")
                    })
                    .inner;
                if discard.clicked() {
                    chosen = Some(FooterAction::Discard);
                }
            }
            if let Some((text, color)) = state {
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
            }
        });
        (None, chosen, provider)
    }
}
