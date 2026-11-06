//! MCP-only credential namespace and ephemeral, redacted resolution.
pub use super::types::{McpCredentialBackend, McpSecretResolver, SecretUnavailableReason};
use super::{McpErrorKind, types::ApprovalScope};
use config::mcp::ValueSource;
use sandbox::{CredentialError, Secret};
use sha2::{Digest, Sha256};

/// Length-framed identities prevent ambiguous concatenation and cannot address a
/// provider credential, even when the logical name is a provider's exact key.
pub fn credential_key(scope: &ApprovalScope, logical_name: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"evorch.mcp.credential.v1\0");
    for field in [
        scope.canonical_root.as_os_str().as_encoded_bytes(),
        scope.server_id.as_bytes(),
        logical_name.as_bytes(),
    ] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    format!("mcp.v1.{:x}", hash.finalize())
}

#[derive(Debug, Clone)]
pub struct SecretResolver {
    backend: McpCredentialBackend,
}
impl SecretResolver {
    pub fn new(backend: McpCredentialBackend) -> Self {
        Self { backend }
    }

    /// GUI may display this classification; no backend detail is retained.
    pub fn resolve_classified(
        &self,
        scope: &ApprovalScope,
        source: &ValueSource,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Secret, SecretUnavailableReason> {
        match source {
            ValueSource::Template { template } => {
                config::mcp::expand::expand_template(template, environment)
                    .map(Secret::from)
                    .map_err(|_| SecretUnavailableReason::Missing)
            }
            ValueSource::Keychain { name } => match &self.backend {
                McpCredentialBackend::Available(store) => store
                    .get(&credential_key(scope, name))
                    .map_err(credential_error_reason)?
                    .ok_or(SecretUnavailableReason::Missing),
                McpCredentialBackend::Unavailable(reason) => Err(*reason),
            },
        }
    }
}
impl McpSecretResolver for SecretResolver {
    fn resolve(
        &self,
        scope: &ApprovalScope,
        source: &ValueSource,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Secret, McpErrorKind> {
        self.resolve_classified(scope, source, environment)
            .map_err(|_| McpErrorKind::SecretUnavailable)
    }
}

pub fn credential_error_reason(error: CredentialError) -> SecretUnavailableReason {
    match error {
        CredentialError::KeychainLocked => SecretUnavailableReason::Locked,
        CredentialError::KeychainCancelled => SecretUnavailableReason::Denied,
        _ => SecretUnavailableReason::Unavailable,
    }
}
