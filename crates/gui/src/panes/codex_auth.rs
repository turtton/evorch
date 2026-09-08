use egui::RichText;

use crate::model::codex_auth::{
    CODEX_AUTHENTICATED_LABEL, CODEX_DEVICE_URL, CODEX_LOGIN_BUTTON, CODEX_REQUESTING_LABEL,
    CODEX_UNAUTHENTICATED_GUIDANCE, CODEX_WAITING_LABEL, CodexAuthModel, CodexAuthState,
    format_expiry,
};
use crate::theme::text::{h4, muted};
use crate::theme::tokens::{ERROR_FG, FONT_H2, FONT_SMALL, SUCCESS};

pub fn codex_auth_section(ui: &mut egui::Ui, model: &CodexAuthModel) -> bool {
    ui.separator();
    ui.label(h4("Codex subscription"));
    match &model.state {
        CodexAuthState::Unauthenticated => {
            ui.label(muted(CODEX_UNAUTHENTICATED_GUIDANCE));
            ui.label(CODEX_DEVICE_URL);
            ui.hyperlink_to("Open device page", CODEX_DEVICE_URL);
        }
        CodexAuthState::Authenticating { prompt: None } => {
            ui.label(CODEX_REQUESTING_LABEL);
        }
        CodexAuthState::Authenticating {
            prompt: Some(prompt),
        } => {
            ui.label(muted("Enter this code at"));
            ui.label(prompt.verification_url.as_str());
            ui.hyperlink_to("Open device page", &prompt.verification_url);
            ui.label(RichText::new(&prompt.user_code).monospace().size(FONT_H2));
            ui.label(CODEX_WAITING_LABEL);
        }
        CodexAuthState::Authenticated { expires_at_unix } => {
            ui.label(RichText::new(CODEX_AUTHENTICATED_LABEL).color(SUCCESS));
            if let Some(expiry) = expires_at_unix {
                ui.label(muted(format_expiry(now_unix(), *expiry)));
            }
        }
        CodexAuthState::Failed { message } => {
            ui.label(
                RichText::new(format!("Codex login failed: {message}"))
                    .color(ERROR_FG)
                    .size(FONT_SMALL),
            );
            ui.label(CODEX_DEVICE_URL);
        }
    }
    ui.label(muted(format!(
        "Tokens live only in the credential store (service \"evorch\", account \"{}\") and are never shown here.",
        model.credential_account
    )));
    ui.add_enabled(
        !model.is_authenticating(),
        egui::Button::new(CODEX_LOGIN_BUTTON).small(),
    )
    .clicked()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
