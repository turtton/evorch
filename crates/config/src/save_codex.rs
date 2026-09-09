//! Named Codex profile persistence and profile removal.

use crate::ConfigError;
use crate::save::{
    insert_profile, invalid_field, normalized_models, read_document, write_document,
};
use std::path::Path;
use toml_edit::{Array, InlineTable, Table, value};

pub struct CodexProviderInput {
    pub name: String,
    pub account: String,
    pub models: Vec<String>,
    pub default_model: String,
}

/// Save a Codex profile without persisting any secret.
///
/// # Errors
/// Returns invalid input, unsupported config versions, or I/O errors.
pub fn save_codex_provider(path: &Path, input: &CodexProviderInput) -> Result<(), ConfigError> {
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
    let models = normalized_models(&input.models);
    if !input.default_model.is_empty() && !models.contains(&input.default_model.as_str()) {
        return Err(invalid_field(
            "providers.default_model",
            "default_model must be a member of models",
        ));
    }
    let mut doc = read_document(path)?;
    let mut profile = Table::new();
    profile.insert("type", value("openai-codex"));
    let mut credential = InlineTable::new();
    credential.insert("type", "keyring".into());
    credential.insert("service", "evorch".into());
    credential.insert("account", input.account.as_str().into());
    profile.insert("credential", value(credential));
    if !models.is_empty() {
        profile.insert("models", value(models.into_iter().collect::<Array>()));
    }
    if !input.default_model.is_empty() {
        profile.insert("default_model", value(input.default_model.as_str()));
    }
    insert_profile(&mut doc, &input.name, profile)?;
    write_document(path, &doc)
}

/// Remove only the named profile. The caller owns credential-store cleanup.
///
/// # Errors
/// Returns invalid config, unsupported versions, or I/O errors.
pub fn delete_provider(path: &Path, name: &str) -> Result<(), ConfigError> {
    let mut doc = read_document(path)?;
    if let Some(providers) = doc.get_mut("providers") {
        let table = providers
            .as_table_like_mut()
            .ok_or_else(|| invalid_field("providers", "providers must be a table"))?;
        table.remove(name);
    }
    write_document(path, &doc)
}
