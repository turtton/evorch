use crate::model::codex_auth::CodexAuthModel;
use crate::model::provider_settings::{
    CredentialMode, OpenAiEditorModel, ProfileEditor, ProviderKind, ProviderSettingsModel,
};
use crate::theme::text::{h3, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{primary_button, surface_frame};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSettingsAction {
    Save,
    Cancel,
    StartCodexLogin,
    RefreshModels,
    Delete(String),
}

pub fn provider_settings_modal(
    ctx: &egui::Context,
    model: &mut ProviderSettingsModel,
    _codex: &CodexAuthModel,
) -> Option<ProviderSettingsAction> {
    let mut action = None;
    let width = (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0;
    egui::Modal::new(egui::Id::new("provider-settings"))
        .backdrop_color(OVERLAY)
        .frame(surface_frame(SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Provider settings"));
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
                        .max_height((ctx.viewport_rect().height() - 160.0).max(100.0))
                        .show(ui, |ui| {
                            action = openai_body(ui, editor, &sources);
                        });
                }
                Some(ProfileEditor::Codex(editor)) => {
                    let busy = editor.auth.is_authenticating();
                    ui.add_enabled_ui(!busy, |ui| {
                        let previous = editor.name.clone();
                        let name = ui.label("Name");
                        ui.text_edit_singleline(&mut editor.name)
                            .labelled_by(name.id);
                        if editor.account == previous {
                            editor.account.clone_from(&editor.name);
                        }
                        let account = ui.label("Account");
                        ui.text_edit_singleline(&mut editor.account)
                            .labelled_by(account.id);
                    });
                    if crate::panes::codex_auth::codex_auth_section(ui, &editor.auth) {
                        action = Some(ProviderSettingsAction::StartCodexLogin);
                    }
                }
                None => {
                    let mut edit = None;
                    for profile in &model.profiles {
                        ui.push_id(&profile.name, |ui| {
                            crate::theme::widgets::compact_row(ui, false, |ui| {
                                ui.label(&profile.name);
                                let label = match profile.provider_type {
                                    config::ProviderTypeConfig::OpenAiCompatible => {
                                        "OpenAI-compatible"
                                    }
                                    config::ProviderTypeConfig::OpenAiCodex => "Codex subscription",
                                    config::ProviderTypeConfig::Anthropic => "Anthropic",
                                    config::ProviderTypeConfig::AnthropicSubscription => {
                                        "Anthropic subscription"
                                    }
                                    config::ProviderTypeConfig::OpenAi => "OpenAI",
                                    config::ProviderTypeConfig::GithubCopilot => "GitHub Copilot",
                                    config::ProviderTypeConfig::Openrouter => "OpenRouter",
                                };
                                crate::theme::widgets::badge(ui, label, ACCENT, SURFACE);
                                ui.label(muted(&profile.default_model));
                                if ui.button("Edit").clicked() {
                                    edit = Some(profile.name.clone());
                                }
                                if ui.button("Delete").clicked() {
                                    model.confirm_delete = Some(profile.name.clone());
                                }
                            });
                            if model.confirm_delete.as_deref() == Some(&profile.name) {
                                ui.horizontal(|ui| {
                                    ui.label(format!("Delete {}?", profile.name));
                                    if ui.button("Confirm delete").clicked() {
                                        action = Some(ProviderSettingsAction::Delete(
                                            profile.name.clone(),
                                        ));
                                    }
                                    if ui.button("Keep profile").clicked() {
                                        model.confirm_delete = None;
                                    }
                                });
                            }
                        });
                    }
                    if let Some(name) = edit {
                        model.edit(&name);
                    }
                    if ui.button("+ Add OpenAI-compatible").clicked() {
                        model.add(ProviderKind::OpenAiCompatible);
                    }
                    if ui.button("+ Add Codex subscription").clicked() {
                        model.add(ProviderKind::CodexSubscription);
                    }
                }
            }
            if let Some(error) = &model.error {
                ui.label(egui::RichText::new(error).color(ERROR_FG));
            }
            ui.horizontal(|ui| {
                if model.editor.is_some() && primary_button(ui, "Save").clicked() {
                    action = Some(ProviderSettingsAction::Save);
                }
                if ui.button("Cancel").clicked() {
                    action = Some(ProviderSettingsAction::Cancel);
                }
            });
        });
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
            .background_color(INPUT),
    )
    .labelled_by(name.id);
    let base = ui.label("Base URL");
    ui.add(
        egui::TextEdit::singleline(&mut model.base_url)
            .desired_width(width)
            .background_color(INPUT),
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
                "•••• stored — enter to replace"
            } else {
                "Paste API key (stored in OS keyring)"
            };
            ui.add(
                egui::TextEdit::singleline(&mut model.api_key_input)
                    .password(true)
                    .hint_text(hint)
                    .desired_width(width)
                    .background_color(INPUT),
            )
            .labelled_by(label.id);
        }
        CredentialMode::Env => {
            let label = ui.label("API key env var");
            ui.add(
                egui::TextEdit::singleline(&mut model.api_key_env)
                    .hint_text("Environment variable NAME, e.g. OPENAI_API_KEY")
                    .desired_width(width)
                    .background_color(INPUT),
            )
            .labelled_by(label.id);
        }
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
            .background_color(INPUT),
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
