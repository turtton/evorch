//! The provider list row, type badge and add-button labels, shared by the embedded workbench
//! and the core client's settings and authentication screens.
//!
//! Pure drawing: no model, runtime or storage reaches here, only the provider type of the
//! configuration or of the protocol.
use crate::theme::tokens::palette;

pub const API_KEY_STORED_HINT: &str = "•••• stored — enter to replace";
pub const API_KEY_EMPTY_HINT: &str = "Paste API key (stored in OS keyring)";

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
