//! Structured errors a core sends instead of free text.

use crate::handshake::{Capability, ProtocolVersion};
use crate::ids::CapabilityName;
use crate::ids::RequestId;
use crate::wire::{InstanceEpoch, Revision};
use serde::{Deserialize, Serialize};

/// Machine-readable failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request is malformed or breaks a field rule.
    InvalidRequest,
    NotFound,
    /// The request conflicts with current state.
    Conflict,
    /// Another client already settled the question or approval.
    AlreadyResolved,
    UnsupportedVersion,
    UnsupportedCapability,
    PermissionDenied,
    /// The entity changed since the revision the client saw.
    StaleRevision,
    /// The core restarted since the epoch the client saw.
    StaleEpoch,
    ResyncRequired,
    Busy,
    Draining,
    LimitExceeded,
    StorageUnavailable,
    RestoreBlocked,
    Internal,
}

/// What the client may do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryAdvice {
    Never,
    /// Resend the identical request (same request id); the core deduplicates.
    SameRequest,
    /// Re-read state with a snapshot, then decide again.
    AfterResync,
    /// Reconnect, then decide again.
    AfterReconnect,
    UserActionRequired,
}

/// Typed extra information; there is deliberately no free-form value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ErrorDetails {
    Field {
        field: String,
    },
    MissingCapabilities {
        capabilities: Vec<CapabilityName>,
    },
    SupportedVersions {
        versions: Vec<ProtocolVersion>,
    },
    Revision {
        expected: Revision,
        actual: Revision,
    },
    Epoch {
        expected: InstanceEpoch,
        actual: InstanceEpoch,
    },
    Limit {
        limit: u32,
    },
}

/// The error a core returns for a failed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
#[serde(deny_unknown_fields)]
pub struct WorkbenchError {
    pub code: ErrorCode,
    /// For people and logs only; clients branch on `code` and `retry`.
    pub message: String,
    pub retry: RetryAdvice,
    /// The request this error answers, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ErrorDetails>,
}

impl WorkbenchError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retry: RetryAdvice) -> Self {
        Self {
            code,
            message: message.into(),
            retry,
            correlation_id: None,
            details: None,
        }
    }

    pub fn for_request(mut self, request_id: RequestId) -> Self {
        self.correlation_id = Some(request_id);
        self
    }

    pub fn with_details(mut self, details: ErrorDetails) -> Self {
        self.details = Some(details);
        self
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message, RetryAdvice::Never)
    }

    pub fn unsupported_capability(capability: Capability) -> Self {
        Self::new(
            ErrorCode::UnsupportedCapability,
            format!("capability {} was not negotiated", capability.name()),
            RetryAdvice::Never,
        )
        .with_details(ErrorDetails::MissingCapabilities {
            capabilities: vec![capability.into()],
        })
    }

    pub fn stale_revision(expected: Revision, actual: Revision) -> Self {
        Self::new(
            ErrorCode::StaleRevision,
            "the entity changed since it was read",
            RetryAdvice::AfterResync,
        )
        .with_details(ErrorDetails::Revision { expected, actual })
    }

    pub fn stale_epoch(expected: InstanceEpoch, actual: InstanceEpoch) -> Self {
        Self::new(
            ErrorCode::StaleEpoch,
            "the core restarted since the epoch the client saw",
            RetryAdvice::AfterReconnect,
        )
        .with_details(ErrorDetails::Epoch { expected, actual })
    }
}

const MAX_ERROR_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_ERROR_LIST: usize = 32;

impl WorkbenchError {
    /// Bound an error received from a peer; its text is relayed, not trusted.
    pub fn validate(&self) -> Result<(), crate::validation::ValidationError> {
        use crate::limits::MAX_NAME_BYTES;
        use crate::validation::{check_bytes, check_count};
        check_bytes(
            "error.message",
            &self.message,
            MAX_ERROR_MESSAGE_BYTES,
            true,
        )?;
        match &self.details {
            Some(ErrorDetails::Field { field }) => {
                check_bytes("error.details.field", field, MAX_NAME_BYTES, true)
            }
            Some(ErrorDetails::MissingCapabilities { capabilities }) => check_count(
                "error.details.capabilities",
                capabilities.len(),
                MAX_ERROR_LIST,
            ),
            Some(ErrorDetails::SupportedVersions { versions }) => {
                check_count("error.details.versions", versions.len(), MAX_ERROR_LIST)
            }
            Some(
                ErrorDetails::Revision { .. }
                | ErrorDetails::Epoch { .. }
                | ErrorDetails::Limit { .. },
            )
            | None => Ok(()),
        }
    }
}
