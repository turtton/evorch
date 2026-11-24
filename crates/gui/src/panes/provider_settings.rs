use super::provider_widgets::{
    ADD_CLAUDE_SUBSCRIPTION, ADD_CODEX, ADD_CURSOR, ADD_KIMI, ADD_OPENAI_COMPATIBLE,
    API_KEY_EMPTY_HINT, API_KEY_STORED_HINT, ProviderRowAction, provider_list_row,
    provider_type_label,
};
use crate::model::codex_auth::CodexAuthModel;
use crate::model::provider_settings::{
    CredentialMode, OpenAiEditorModel, ProfileEditor, ProviderKind, ProviderSettingsModel,
};
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::primary_button;

#[path = "provider_codex_editor.rs"]
mod codex_editor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSettingsAction {
    Save,
    Cancel,
    StartCodexLogin,
    StartSubscriptionLogin,
    CompleteSubscriptionLogin,
    CancelSubscriptionLogin,
    RefreshModels,
    Delete(String),
}

/// The Providers tab of the settings modal. Closing the modal is the modal's own action;
/// `Cancel` here only leaves the editor for the profile list.
pub fn provider_settings_body(
    ui: &mut egui::Ui,
    model: &mut ProviderSettingsModel,
    _codex: &CodexAuthModel,
) -> Option<ProviderSettingsAction> {
    let mut action = None;
    let ctx = ui.ctx().clone();
    {
        {
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            super::model_metadata::catalog_toolbar(ui, &mut model.catalog);
            let sources = crate::model::model_metadata::MetadataSources {
                presets: &model.model_presets,
                catalog: model.catalog.catalog.as_deref(),
            };
            match &mut model.editor {
                Some(ProfileEditor::OpenAiCompatible(editor)) => {
                    egui::ScrollArea::vertical()
                        .id_salt("openai-editor")
                        .auto_shrink([false, false])
                        // Break the sizing feedback loop with the modal's previous-frame height.
                        .min_scrolled_height((ctx.viewport_rect().height() - 240.0).max(100.0))
                        .max_height((ctx.viewport_rect().height() - 240.0).max(100.0))
                        .show(ui, |ui| {
                            action = openai_body(ui, editor, &sources);
                        });
                }
                Some(ProfileEditor::Subscription(editor)) => {
                    egui::ScrollArea::vertical()
                        .id_salt("subscription-editor")
                        .auto_shrink([false, false])
                        .min_scrolled_height((ctx.viewport_rect().height() - 240.0).max(100.0))
                        .max_height((ctx.viewport_rect().height() - 240.0).max(100.0))
                        .show(ui, |ui| {
                            action = super::subscription_provider::subscription_body(
                                ui, editor, &sources,
                            );
                        });
                }
                Some(ProfileEditor::Codex(editor)) => {
                    egui::ScrollArea::vertical()
                        .id_salt("codex-editor")
                        .auto_shrink([false, false])
                        .max_height((ctx.viewport_rect().height() - 240.0).max(100.0))
                        .show(ui, |ui| {
                            if let Some(editor_action) = codex_editor::codex_body(ui, editor) {
                                action = Some(editor_action);
                            }
                        });
                }
                None => {
                    let mut edit = None;
                    for profile in &model.profiles {
                        match provider_list_row(
                            ui,
                            &profile.name,
                            provider_type_label(profile.provider_type),
                            &profile.default_model,
                            None,
                            &mut model.confirm_delete,
                        ) {
                            Some(ProviderRowAction::Edit) => edit = Some(profile.name.clone()),
                            Some(ProviderRowAction::Delete) => {
                                action = Some(ProviderSettingsAction::Delete(profile.name.clone()));
                            }
                            None => {}
                        }
                    }
                    if let Some(name) = edit {
                        model.edit(&name);
                    }
                    if ui.button(ADD_OPENAI_COMPATIBLE).clicked() {
                        model.add(ProviderKind::OpenAiCompatible);
                    }
                    if ui.button(ADD_CODEX).clicked() {
                        model.add(ProviderKind::CodexSubscription);
                    }
                    if ui.button("+ Add Claude API").clicked() {
                        model.add(ProviderKind::ClaudeApi);
                    }
                    if ui.button(ADD_CLAUDE_SUBSCRIPTION).clicked() {
                        model.add(ProviderKind::ClaudeSubscription);
                    }
                    if ui.button(ADD_CURSOR).clicked() {
                        model.add(ProviderKind::Cursor);
                    }
                    if ui.button(ADD_KIMI).clicked() {
                        model.add(ProviderKind::KimiSubscription);
                    }
                }
            }
            if let Some(error) = &model.error {
                crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
            }
            ui.horizontal(|ui| {
                if model.editor.is_some() {
                    let busy = matches!(&model.editor, Some(ProfileEditor::Subscription(editor)) if editor.busy());
                    ui.add_enabled_ui(!busy, |ui| {
                        if primary_button(ui, "Save").clicked() { action = Some(ProviderSettingsAction::Save); }
                    });
                }
                if model.editor.is_some() && ui.button("Back to list").clicked() {
                    action = Some(ProviderSettingsAction::Cancel);
                }
            });
        }
    }
    action
}

fn openai_body(
    ui: &mut egui::Ui,
    model: &mut OpenAiEditorModel,
    sources: &crate::model::model_metadata::MetadataSources<'_>,
) -> Option<ProviderSettingsAction> {
    let mut action = None;
    let width = ui.available_width();
    let name = ui.label("Name");
    ui.add(
        egui::TextEdit::singleline(&mut model.name)
            .desired_width(width)
            .background_color(palette().INPUT),
    )
    .labelled_by(name.id);
    let base = ui.label("Base URL");
    ui.add(
        egui::TextEdit::singleline(&mut model.base_url)
            .desired_width(width)
            .background_color(palette().INPUT),
    )
    .labelled_by(base.id);
    ui.horizontal(|ui| {
        ui.radio_value(
            &mut model.credential_mode,
            CredentialMode::Keyring,
            "Keyring (recommended)",
        );
        ui.radio_value(
            &mut model.credential_mode,
            CredentialMode::Env,
            "Environment variable",
        );
    });
    match model.credential_mode {
        CredentialMode::Keyring => {
            let label = ui.label("API key");
            let hint = if model.api_key_stored {
                API_KEY_STORED_HINT
            } else {
                API_KEY_EMPTY_HINT
            };
            ui.add(
                egui::TextEdit::singleline(&mut model.api_key_input)
                    .password(true)
                    .hint_text(hint)
                    .desired_width(width)
                    .background_color(palette().INPUT),
            )
            .labelled_by(label.id);
        }
        CredentialMode::Env => {
            let label = ui.label("API key env var");
            ui.add(
                egui::TextEdit::singleline(&mut model.api_key_env)
                    .hint_text("Environment variable NAME, e.g. OPENAI_API_KEY")
                    .desired_width(width)
                    .background_color(palette().INPUT),
            )
            .labelled_by(label.id);
        }
    }
    if model.provider_type == config::ProviderTypeConfig::Anthropic {
        ui.label(muted("API-key subscription quota is unavailable; API rate limits are separate from Claude subscription usage."));
    }
    if super::provider_models::provider_models(ui, model, sources) {
        action = Some(ProviderSettingsAction::RefreshModels);
    }
    let excluded = ui.label("Excluded models");
    ui.add(
        egui::TextEdit::multiline(&mut model.excluded_models_text)
            .hint_text("one per line")
            .desired_rows(3)
            .desired_width(width)
            .background_color(palette().INPUT),
    )
    .labelled_by(excluded.id);
    let label = ui.label("Default model");
    let choices = model.candidate_models();
    egui::ComboBox::from_id_salt("provider-default-model")
        .width(width)
        .selected_text(&model.default_model)
        .show_ui(ui, |ui| {
            for candidate in choices {
                ui.selectable_value(&mut model.default_model, candidate.clone(), candidate);
            }
        })
        .response
        .labelled_by(label.id);
    ui.add(
        egui::Label::new(muted(
            "Used when a route doesn't override the model and when re-resolving a pinned session.",
        ))
        .wrap(),
    );
    action
}
