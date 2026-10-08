//! Secret-free persistence for Claude OAuth and Cursor profiles.

use crate::{
    ConfigError, OpenAiCompatibleProviderInput, ProviderCredentialInput, ProviderTypeConfig,
};
use std::path::Path;

pub struct SubscriptionProviderInput {
    pub name: String,
    pub provider_type: ProviderTypeConfig,
    pub account: String,
    pub base_url: String,
    pub models: Vec<crate::ModelEntryConfig>,
    pub excluded_models: Vec<String>,
    pub default_model: String,
}

/// Save a subscription profile atomically, including a profile rename.
///
/// # Errors
/// Rejects unsupported types, invalid fields, invalid config and I/O failures.
pub fn save_subscription_provider_edit(
    path: &Path,
    input: &SubscriptionProviderInput,
    original_name: Option<&str>,
) -> Result<(), ConfigError> {
    if !matches!(
        input.provider_type,
        ProviderTypeConfig::AnthropicSubscription | ProviderTypeConfig::Cursor
    ) {
        return Err(crate::save::invalid_field(
            "providers.type",
            "expected Claude subscription or Cursor",
        ));
    }
    let validated = OpenAiCompatibleProviderInput {
        name: input.name.clone(),
        provider_type: input.provider_type,
        base_url: input.base_url.clone(),
        credential: ProviderCredentialInput::Keyring {
            service: "evorch".into(),
            account: input.account.clone(),
        },
        models: input.models.clone(),
        excluded_models: input.excluded_models.clone(),
        default_model: input.default_model.clone(),
    };
    crate::save::save_openai_compatible_provider_edit(path, &validated, original_name)
}
