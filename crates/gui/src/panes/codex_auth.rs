use egui::RichText;

use crate::model::codex_auth::{
    CODEX_AUTHENTICATED_LABEL, CODEX_LOGIN_BUTTON, CODEX_REQUESTING_LABEL,
    CODEX_UNAUTHENTICATED_GUIDANCE, CODEX_WAITING_LABEL, CodexAuthError, CodexAuthModel,
    CodexAuthState, format_expiry,
};
use crate::theme::text::{h4, muted};
use crate::theme::tokens::{ERROR_FG, FONT_SMALL, SUCCESS};

pub const CODEX_LOGIN_FAILED_NETWORK: &str =
    "Codex login failed: network unavailable. Please retry.";
pub const CODEX_LOGIN_FAILED_STORE: &str = "Codex login failed: credential store unavailable.";
pub const CODEX_LOGIN_FAILED_REJECTED: &str = "Codex login failed: authorization rejected.";
pub const CODEX_LOGIN_FAILED_UNAVAILABLE: &str = "Codex login is unavailable. Please retry later.";

pub fn codex_auth_section(ui: &mut egui::Ui, model: &CodexAuthModel) -> bool {
    ui.separator();
    ui.label(h4("Codex subscription"));
    match &model.state {
        CodexAuthState::Unauthenticated => {
            ui.label(muted(CODEX_UNAUTHENTICATED_GUIDANCE));
        }
        CodexAuthState::Authenticating { prompt: None, .. } => {
            ui.label(CODEX_REQUESTING_LABEL);
        }
        CodexAuthState::Authenticating {
            prompt: Some(prompt),
            ..
        } => {
            ui.label(CODEX_WAITING_LABEL);
            ui.hyperlink_to("Open the sign-in page again", &prompt.authorize_url);
        }
        CodexAuthState::Authenticated { expires_at_unix } => {
            ui.label(RichText::new(CODEX_AUTHENTICATED_LABEL).color(SUCCESS));
            if let Some(expiry) = expires_at_unix {
                ui.label(muted(format_expiry(now_unix(), *expiry)));
            }
        }
        CodexAuthState::Failed { failure } => {
            let message = match failure {
                CodexAuthError::Network => CODEX_LOGIN_FAILED_NETWORK,
                CodexAuthError::StoreUnavailable => CODEX_LOGIN_FAILED_STORE,
                CodexAuthError::Rejected => CODEX_LOGIN_FAILED_REJECTED,
                CodexAuthError::Unavailable => CODEX_LOGIN_FAILED_UNAVAILABLE,
                CodexAuthError::CallbackPortBusy => {
                    "Codex callback ports 1455 and 1457 are busy. Close the other login and retry."
                }
                CodexAuthError::Timeout => "Codex browser sign-in timed out. Please retry.",
            };
            ui.label(RichText::new(message).color(ERROR_FG).size(FONT_SMALL));
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
