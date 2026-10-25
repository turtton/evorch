//! Named Codex profile persistence and profile removal.

use crate::ConfigError;
use crate::save::{
    insert_profile, invalid_field, model_values, normalized_models, read_document,
    remove_original_profile,
};
use crate::save_context::SaveScope;
use crate::types::provider::ModelEntryConfig;
use std::path::Path;
use toml_edit::{InlineTable, Table, value};

pub struct CodexProviderInput {
    pub name: String,
    pub account: String,
    pub base_url: String,
    pub models: Vec<ModelEntryConfig>,
    pub default_model: String,
}

/// Save a Codex profile without persisting any secret.
///
/// # Errors
/// Returns invalid input, unsupported config versions, or I/O errors.
pub fn save_codex_provider(path: &Path, input: &CodexProviderInput) -> Result<(), ConfigError> {
    save_codex_provider_edit(path, input, None)
}

/// Save a Codex profile, replacing its original name in the same atomic write.
///
/// # Errors
/// Returns invalid input, unsupported config versions, or I/O errors.
pub fn save_codex_provider_edit(
    path: &Path,
    input: &CodexProviderInput,
    original_name: Option<&str>,
) -> Result<(), ConfigError> {
    save_codex_provider_edit_inner(
        path,
        input,
        original_name,
        SaveScope::Standalone,
        if input.models.is_empty() && input.default_model.is_empty() {
            CodexModelIntent::BuiltinDefaults
        } else {
            CodexModelIntent::ExplicitModels
        },
    )
}

/// Save a Codex GUI edit after validating all effective role profiles.
/// An explicit chat default is required, even when the edited model list is empty.
///
/// # Errors
/// Rejects invalid input or effective model kinds before writing anything.
pub fn save_codex_provider_edit_with_options(
    path: &Path,
    input: &CodexProviderInput,
    original_name: Option<&str>,
    options: &crate::LoadOptions,
) -> Result<(), ConfigError> {
    save_codex_provider_edit_inner(
        path,
        input,
        original_name,
        SaveScope::Layered(options),
        CodexModelIntent::ExplicitModels,
    )
}

/// Whether an empty model list intentionally requests Codex built-in defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexModelIntent {
    BuiltinDefaults,
    ExplicitModels,
}

pub(crate) fn save_codex_provider_edit_inner(
    path: &Path,
    input: &CodexProviderInput,
    original_name: Option<&str>,
    scope: SaveScope<'_>,
    intent: CodexModelIntent,
) -> Result<(), ConfigError> {
    if input.name.is_empty()
        || !input
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(invalid_field(
            "providers.name",
            "name must match ^[A-Za-z0-9_-]+$",
        ));
    }
    if input.account.trim().is_empty() {
        return Err(invalid_field(
            "providers.credential.account",
            "keyring account must not be empty",
        ));
    }
    let base_url = input.base_url.trim();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(invalid_field(
            "providers.base_url",
            "base_url must start with http:// or https://",
        ));
    }
    let models = normalized_models(&input.models);
    if !input.default_model.is_empty()
        && !models.iter().any(|model| model.id == input.default_model)
    {
        return Err(invalid_field(
            "providers.default_model",
            "default_model must be a member of models",
        ));
    }
    if models.iter().any(|model| {
        model.id == input.default_model
            && model.effective_kind() == crate::ModelKind::ImageGeneration
    }) {
        return Err(invalid_field(
            "providers.default_model",
            crate::save::DEFAULT_MODEL_KIND_MESSAGE,
        ));
    }
    if intent == CodexModelIntent::BuiltinDefaults
        && (!models.is_empty() || !input.default_model.is_empty())
    {
        return Err(invalid_field(
            "providers.models",
            "built-in defaults require empty models and default_model",
        ));
    }
    if intent == CodexModelIntent::ExplicitModels
        && !models.iter().any(|model| {
            model.id == input.default_model
                && model.enabled
                && model.effective_kind() == crate::ModelKind::Chat
        })
    {
        return Err(invalid_field(
            "providers.default_model",
            "Codex edits require an enabled chat default_model; add a chat model before saving",
        ));
    }
    let mut doc = read_document(path)?;
    let mut profile = Table::new();
    profile.insert("type", value("openai-codex"));
    profile.insert("api_protocol", value("openai-codex-responses"));
    profile.insert("base_url", value(base_url));
    let mut credential = InlineTable::new();
    credential.insert("type", "keyring".into());
    credential.insert("service", "evorch".into());
    credential.insert("account", input.account.as_str().into());
    profile.insert("credential", value(credential));
    if !models.is_empty() {
        profile.insert("models", value(model_values(&models)?));
    }
    if !input.default_model.is_empty() {
        profile.insert("default_model", value(input.default_model.as_str()));
    }
    insert_profile(&mut doc, &input.name, original_name, profile)?;
    remove_original_profile(&mut doc, original_name, &input.name);
    crate::save::write_document_in_scope(path, &doc, scope)
}

/// Remove only the named profile. The caller owns credential-store cleanup.
///
/// # Errors
/// Returns invalid config, unsupported versions, or I/O errors.
pub fn delete_provider(path: &Path, name: &str) -> Result<(), ConfigError> {
    delete_provider_in_scope(path, name, SaveScope::Standalone)
}

pub(crate) fn delete_provider_in_scope(
    path: &Path,
    name: &str,
    scope: SaveScope<'_>,
) -> Result<(), ConfigError> {
    let mut doc = read_document(path)?;
    if let Some(providers) = doc.get_mut("providers") {
        let table = providers
            .as_table_like_mut()
            .ok_or_else(|| invalid_field("providers", "providers must be a table"))?;
        table.remove(name);
    }
    crate::save::write_document_in_scope(path, &doc, scope)
}
