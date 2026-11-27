//! The provider list row, type badge and add-button labels, shared by the embedded workbench
//! and the core client's settings and authentication screens.
//!
//! Pure drawing: no model, runtime or storage reaches here, only the provider type of the
//! configuration or of the protocol.
use crate::theme::text::muted;
use crate::theme::tokens::palette;

pub const ADD_OPENAI_COMPATIBLE: &str = "+ Add OpenAI-compatible";
pub const ADD_CODEX: &str = "+ Add Codex subscription";
pub const ADD_CLAUDE_SUBSCRIPTION: &str = "+ Add Claude subscription";
pub const ADD_CURSOR: &str = "+ Add Cursor";
pub const ADD_KIMI: &str = "+ Add Kimi subscription (beta, second push)";
pub const API_KEY_STORED_HINT: &str = "•••• stored — enter to replace";
pub const API_KEY_EMPTY_HINT: &str = "Paste API key (stored in OS keyring)";
pub const EDIT_IN_CONFIG_FILE: &str =
    "This provider type must be edited in config.toml; its configuration is preserved.";

pub const fn provider_type_label(kind: config::ProviderTypeConfig) -> &'static str {
    match kind {
        config::ProviderTypeConfig::OpenAiCompatible => "OpenAI-compatible",
        config::ProviderTypeConfig::OpenAiCodex => "Codex subscription",
        config::ProviderTypeConfig::Anthropic => "Claude API",
        config::ProviderTypeConfig::Cursor => "Cursor",
        config::ProviderTypeConfig::AnthropicSubscription => "Claude subscription",
        config::ProviderTypeConfig::OpenAi => "OpenAI",
        config::ProviderTypeConfig::GithubCopilot => "GitHub Copilot",
        config::ProviderTypeConfig::Openrouter => "OpenRouter",
        config::ProviderTypeConfig::KimiSubscription => "Kimi subscription",
    }
}

pub const fn provider_kind_label(
    kind: workbench_protocol::namespaces::settings::ProviderKind,
) -> &'static str {
    use workbench_protocol::namespaces::settings::ProviderKind as Kind;
    provider_type_label(match kind {
        Kind::OpenaiCompatible => config::ProviderTypeConfig::OpenAiCompatible,
        Kind::OpenaiCodex => config::ProviderTypeConfig::OpenAiCodex,
        Kind::Anthropic => config::ProviderTypeConfig::Anthropic,
        Kind::Cursor => config::ProviderTypeConfig::Cursor,
        Kind::AnthropicSubscription => config::ProviderTypeConfig::AnthropicSubscription,
        Kind::Openai => config::ProviderTypeConfig::OpenAi,
        Kind::GithubCopilot => config::ProviderTypeConfig::GithubCopilot,
        Kind::Openrouter => config::ProviderTypeConfig::Openrouter,
        Kind::KimiSubscription => config::ProviderTypeConfig::KimiSubscription,
    })
}

pub fn provider_type_badge(ui: &mut egui::Ui, label: &str) {
    crate::theme::widgets::badge(ui, label, palette().ACCENT, palette().SURFACE);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderRowAction {
    Edit,
    Delete,
}

pub fn provider_list_row(
    ui: &mut egui::Ui,
    name: &str,
    type_label: &str,
    default_model: &str,
    edit_disabled: Option<&str>,
    confirm_delete: &mut Option<String>,
) -> Option<ProviderRowAction> {
    let mut action = None;
    ui.push_id(name, |ui| {
        crate::theme::widgets::compact_row(ui, false, |ui| {
            ui.label(name);
            provider_type_badge(ui, type_label);
            ui.label(muted(default_model));
            let edit = ui.add_enabled(edit_disabled.is_none(), egui::Button::new("Edit"));
            let edit = match edit_disabled {
                Some(reason) => edit.on_disabled_hover_text(reason),
                None => edit,
            };
            if edit.clicked() {
                action = Some(ProviderRowAction::Edit);
            }
            if ui.button("Delete").clicked() {
                *confirm_delete = Some(name.to_owned());
            }
        });
        if confirm_delete.as_deref() == Some(name) {
            ui.horizontal(|ui| {
                ui.label(format!("Delete {name}?"));
                if ui.button("Confirm delete").clicked() {
                    action = Some(ProviderRowAction::Delete);
                }
                if ui.button("Keep profile").clicked() {
                    *confirm_delete = None;
                }
            });
        }
    });
    action
}
