//! Model discovery for one saved provider (settings PR 2).
//!
//! The client names a provider that is already in the saved file; the core asks
//! the provider's endpoint for its models with the credential it already holds.
//! Nothing here carries a URL, a header or a secret: a client cannot point the
//! core at an address, and the answer is only model ids.
//!
//! The query handler of a host cannot wait for the network, so discovery is a
//! command: `fetch_provider_models` starts a tracked operation (the shared
//! contract of [`crate::operation`]) and answers at once with it `pending`. When
//! the operation is `Ready`, `get_provider_models` reads what it found. The
//! result is kept as long as the operation is in the core's table. A provider
//! kind with no discovery ends the operation `Failed` with
//! `unsupported_capability`; a provider that does not answer in time ends it
//! `Failed` too. Discovery changes no setting: the client decides what to add
//! and saves it with `save_provider`.
use super::{MAX_MODELS, SettingsRevision};
use crate::{
    ids::OperationId,
    limits::MAX_NAME_BYTES,
    validation::{ValidationError, check_count, check_line},
};
use serde::{Deserialize, Serialize};

/// Ask the provider `provider` of the file at `expected_revision` for its models.
/// A file that changed since is refused with `stale_revision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchProviderModels {
    pub operation_id: OperationId,
    pub expected_revision: SettingsRevision,
    /// The profile name in the saved file.
    pub provider: String,
}

impl FetchProviderModels {
    pub(super) fn validate(&self) -> Result<(), ValidationError> {
        check_line("provider", &self.provider, MAX_NAME_BYTES, false)
    }
}

/// The model ids a provider offered, sorted and without duplicates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelCandidates {
    pub operation_id: OperationId,
    pub provider: String,
    /// At most [`MAX_MODELS`] ids.
    pub models: Vec<String>,
    /// The provider offered more than `models` holds.
    pub truncated: bool,
}

impl ProviderModelCandidates {
    pub(super) fn validate(&self) -> Result<(), ValidationError> {
        check_line("provider", &self.provider, MAX_NAME_BYTES, false)?;
        check_count("models", self.models.len(), MAX_MODELS)?;
        for model in &self.models {
            check_line("model", model, MAX_NAME_BYTES, false)?;
        }
        Ok(())
    }
}
