use super::provider_settings::ProviderSettingsAction;
use crate::{
    model::{
        model_metadata::MetadataSources,
        subscription_provider::{SubscriptionAuthState, SubscriptionEditorModel, unix_now},
    },
    theme::{text::muted, tokens::palette},
};

pub fn subscription_body(
    ui: &mut egui::Ui,
    editor: &mut SubscriptionEditorModel,
    sources: &MetadataSources<'_>,
) -> Option<ProviderSettingsAction> {
    let mut action = None;
    let busy = editor.busy();
    ui.add_enabled_ui(!busy, |ui| {
        let previous = editor.models.name.clone();
        let label = ui.label("Name");
        ui.text_edit_singleline(&mut editor.models.name)
            .labelled_by(label.id);
        editor.rename_profile(&previous);
        let label = ui.label("Credential account");
        ui.text_edit_singleline(&mut editor.account)
            .labelled_by(label.id);
        let label = ui.label("Base URL");
        ui.text_edit_singleline(&mut editor.models.base_url)
            .labelled_by(label.id);
    });
    match &editor.auth {
        SubscriptionAuthState::SignedOut => {
            ui.label(muted(format!(
                "Sign in to {} in your browser",
                editor.service()
            )));
        }
        SubscriptionAuthState::Waiting { manual_code } => {
            ui.spinner();
            ui.label(muted("Waiting for browser sign-in…"));
            if let Some(url) = &editor.authorize_url {
                ui.hyperlink_to("Open login page", url);
            }
            if *manual_code {
                ui.add(egui::Label::new(muted("The browser callback completes login automatically. If it cannot return, paste the authorization code or final callback URL below.")).wrap());
                let label = ui.label("Authorization code / callback URL");
                ui.add(egui::TextEdit::singleline(&mut editor.code_input).password(true))
                    .labelled_by(label.id);
                if ui
                    .add_enabled(
                        !editor.code_input.trim().is_empty(),
                        egui::Button::new("Complete sign-in"),
                    )
                    .clicked()
                {
                    action = Some(ProviderSettingsAction::CompleteSubscriptionLogin);
                }
            }
        }
        SubscriptionAuthState::Exchanging => {
            ui.spinner();
            ui.label(muted("Completing sign-in…"));
        }
        SubscriptionAuthState::SignedIn { expires_at } => {
            let expiry = i64::try_from(*expires_at)
                .ok()
                .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
                .map_or_else(
                    || "unknown".into(),
                    |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
                );
            ui.label(muted(format!("Signed in · token expires {expiry}")));
            if *expires_at <= unix_now() {
                ui.label(muted(
                    "Token expired; it will refresh automatically when used.",
                ));
            }
        }
        SubscriptionAuthState::Failed(error) => {
            ui.colored_label(palette().ERROR_FG, *error);
        }
    }
    if busy {
        if ui.button("Cancel sign-in").clicked() {
            action = Some(ProviderSettingsAction::CancelSubscriptionLogin);
        }
    } else if ui
        .button(format!("Sign in to {}", editor.service()))
        .clicked()
    {
        action = Some(ProviderSettingsAction::StartSubscriptionLogin);
    }
    ui.add_enabled_ui(!busy, |ui| {
        if super::provider_models::provider_models(ui, &mut editor.models, sources) {
            action = Some(ProviderSettingsAction::RefreshModels);
        }
        let label = ui.label("Excluded models");
        ui.add(egui::TextEdit::multiline(&mut editor.models.excluded_models_text).desired_rows(2))
            .labelled_by(label.id);
        let choices = editor.models.candidate_models();
        let label = ui.label("Default model");
        egui::ComboBox::from_id_salt("subscription-default-model")
            .selected_text(&editor.models.default_model)
            .show_ui(ui, |ui| {
                for id in choices {
                    ui.selectable_value(&mut editor.models.default_model, id.clone(), id);
                }
            })
            .response
            .labelled_by(label.id);
    });
    action
}
