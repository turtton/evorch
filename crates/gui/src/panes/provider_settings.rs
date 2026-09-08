use crate::model::codex_auth::CodexAuthModel;
use crate::model::provider_settings::{CredentialMode, ModelsFetchState, ProviderSettingsModel, ProviderSettingsTab};
use crate::theme::text::{h3, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{primary_button, surface_frame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSettingsAction {
    Save,
    Cancel,
    StartCodexLogin,
    RefreshModels,
}

pub fn provider_settings_modal(
    ctx: &egui::Context,
    model: &mut ProviderSettingsModel,
    codex: &CodexAuthModel,
) -> Option<ProviderSettingsAction> {
    let mut action = None;
    let width = (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 2.0;
    egui::Modal::new(egui::Id::new("provider-settings"))
        .backdrop_color(OVERLAY)
        .frame(surface_frame(SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Provider settings"));
            surface_frame(SURFACE).corner_radius(R_PILL).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut model.tab, ProviderSettingsTab::OpenAi, "OpenAI-compatible");
                    ui.selectable_value(&mut model.tab, ProviderSettingsTab::Codex, "Codex subscription");
                });
            });
            match model.tab {
                ProviderSettingsTab::OpenAi => { action = openai_body(ui, model); }
                ProviderSettingsTab::Codex => {
                    if crate::panes::codex_auth::codex_auth_section(ui, codex) {
                        action = Some(ProviderSettingsAction::StartCodexLogin);
                    }
                }
            }
            if let Some(error) = &model.error {
                ui.label(egui::RichText::new(error).color(ERROR_FG));
            }
            ui.horizontal(|ui| {
                ui.add_enabled_ui(model.tab == ProviderSettingsTab::OpenAi, |ui| {
                    if primary_button(ui, "Save").clicked() { action = Some(ProviderSettingsAction::Save); }
                });
                if ui.button("Cancel").clicked() { action = Some(ProviderSettingsAction::Cancel); }
            });
        });
    action
}

fn openai_body(ui: &mut egui::Ui, model: &mut ProviderSettingsModel) -> Option<ProviderSettingsAction> {
    let mut action = None;
    let width = ui.available_width();
    let name = ui.label("Name");
    ui.add(egui::TextEdit::singleline(&mut model.name).desired_width(width).background_color(INPUT)).labelled_by(name.id);
    let base = ui.label("Base URL");
    ui.add(egui::TextEdit::singleline(&mut model.base_url).desired_width(width).background_color(INPUT)).labelled_by(base.id);
    ui.horizontal(|ui| {
        ui.radio_value(&mut model.credential_mode, CredentialMode::Keyring, "Keyring (recommended)");
        ui.radio_value(&mut model.credential_mode, CredentialMode::Env, "Environment variable");
    });
    match model.credential_mode {
        CredentialMode::Keyring => {
            let label = ui.label("API key");
            let hint = if model.api_key_stored { "•••• stored — enter to replace" } else { "Paste API key (stored in OS keyring)" };
            ui.add(egui::TextEdit::singleline(&mut model.api_key_input).password(true).hint_text(hint).desired_width(width).background_color(INPUT)).labelled_by(label.id);
        }
        CredentialMode::Env => {
            let label = ui.label("API key env var");
            ui.add(egui::TextEdit::singleline(&mut model.api_key_env).hint_text("Environment variable NAME, e.g. OPENAI_API_KEY").desired_width(width).background_color(INPUT)).labelled_by(label.id);
        }
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button("Refresh models").clicked() { action = Some(ProviderSettingsAction::RefreshModels); }
        match &model.models_fetch_state {
            ModelsFetchState::Idle => {}
            ModelsFetchState::Loading => { ui.label(muted("Loading models…")); }
            ModelsFetchState::Loaded => { ui.label(muted(format!("Loaded {} models from /v1/models", model.available_models.as_ref().map_or(0, Vec::len)))); }
            ModelsFetchState::Failed(error) => { ui.label(egui::RichText::new(format!("Auto-fetch failed ({error}); manual entry below")).color(ERROR_FG).size(FONT_SMALL)); }
        }
    });
    let models = ui.label("Models");
    ui.add(egui::TextEdit::multiline(&mut model.models_text).hint_text("one per line").desired_rows(3).desired_width(width).background_color(INPUT)).labelled_by(models.id);
    let excluded = ui.label("Excluded models");
    ui.add(egui::TextEdit::multiline(&mut model.excluded_models_text).hint_text("one per line").desired_rows(3).desired_width(width).background_color(INPUT)).labelled_by(excluded.id);
    let label = ui.label("Default model");
    let choices = model.candidate_models();
    egui::ComboBox::from_id_salt("provider-default-model").width(width).selected_text(&model.default_model).show_ui(ui, |ui| {
        for candidate in choices { ui.selectable_value(&mut model.default_model, candidate.clone(), candidate); }
    }).response.labelled_by(label.id);
    ui.add(egui::Label::new(muted("Used when a route doesn't override the model and when re-resolving a pinned session.")).wrap());
    action
}
