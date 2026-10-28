use super::provider_settings::ProviderSettingsAction;
use crate::{
    model::{
        model_metadata::MetadataSources,
        subscription_provider::{
            SubscriptionAuthMethod, SubscriptionAuthState, SubscriptionEditorModel, unix_now,
        },
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
    let cursor = editor.models.provider_type == config::ProviderTypeConfig::Cursor;
    if cursor {
        ui.add_enabled_ui(!busy, |ui| {
            let mut method = editor.auth_method;
            ui.horizontal(|ui| {
                ui.label("Authentication");
                ui.selectable_value(
                    &mut method,
                    SubscriptionAuthMethod::Browser,
                    "Browser sign-in",
                );
                ui.selectable_value(&mut method, SubscriptionAuthMethod::ApiKey, "API key");
            });
            editor.set_auth_method(method);
            if editor.auth_method == SubscriptionAuthMethod::ApiKey {
                ui.hyperlink_to(
                    "Create a Cursor user API key",
                    "https://cursor.com/dashboard/api",
                );
                let label = ui.label("Cursor API key");
                ui.add(egui::TextEdit::singleline(&mut editor.api_key_input).password(true))
                    .labelled_by(label.id);
                ui.label(muted(
                    "Stored in the credential store, never in config.toml. Uses your Cursor plan.",
                ));
            }
        });
    }
    let api_key = cursor && editor.auth_method == SubscriptionAuthMethod::ApiKey;
    match &editor.auth {
        SubscriptionAuthState::SignedOut => {
            ui.label(muted(if api_key {
                "Authenticate with a Cursor user API key".into()
            } else {
                format!("Sign in to {} in your browser", editor.service())
            }));
        }
        SubscriptionAuthState::Waiting { manual_code } => {
            crate::theme::widgets::spinner(ui);
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
            crate::theme::widgets::spinner(ui);
            ui.label(muted("Completing sign-in…"));
        }
        SubscriptionAuthState::SignedIn { expires_at } => {
            let now = unix_now();
            let (label, absolute) = token_expiry(*expires_at, now);
            let response = ui.label(muted(label));
            if let Some(absolute) = absolute {
                response.on_hover_text(absolute);
            }
            if *expires_at <= now {
                ui.label(muted(
                    "Token expired; it will renew automatically when used.",
                ));
            }
        }
        SubscriptionAuthState::Failed(error) => {
            crate::theme::widgets::reason_label(ui, palette().ERROR_FG, *error);
        }
    }
    if busy {
        if ui.button("Cancel sign-in").clicked() {
            action = Some(ProviderSettingsAction::CancelSubscriptionLogin);
        }
    } else {
        let label = if api_key {
            "Authenticate with API key".into()
        } else {
            format!("Sign in to {}", editor.service())
        };
        if ui
            .add_enabled(
                !api_key || !editor.api_key_input.trim().is_empty(),
                egui::Button::new(label),
            )
            .clicked()
        {
            action = Some(ProviderSettingsAction::StartSubscriptionLogin);
        }
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

/// Relative expiry for the label, with the absolute UTC time for its tooltip.
fn token_expiry(expires_at: u64, now: u64) -> (String, Option<String>) {
    let at = |seconds: u64| {
        i64::try_from(seconds)
            .ok()
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
    };
    match (at(expires_at), at(now)) {
        (Some(expiry), Some(now)) if expiry > now => (
            format!(
                "Signed in · token expires {}",
                crate::format::time_until(expiry, now)
            ),
            Some(crate::format::utc_timestamp(expiry)),
        ),
        (Some(expiry), _) => (
            "Signed in · token expired".into(),
            Some(crate::format::utc_timestamp(expiry)),
        ),
        (None, _) => ("Signed in · token expiry unknown".into(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::token_expiry;

    #[test]
    fn token_expiry_reads_as_remaining_time_with_absolute_tooltip() {
        let now = 1_791_000_000;
        assert_eq!(
            token_expiry(now + 5 * 3_600 + 20 * 60, now),
            (
                "Signed in · token expires in 5h 20m".into(),
                Some("2026-10-03 09:20 UTC".into())
            )
        );
        assert_eq!(token_expiry(now, now).0, "Signed in · token expired");
        assert_eq!(
            token_expiry(u64::MAX, now),
            ("Signed in · token expiry unknown".into(), None)
        );
    }
}
