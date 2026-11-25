//! Why an artifact could not be shown, told apart for the reader.
//!
//! The core answers a missing artifact, a wrong digest and a foreign thread with
//! one `NotFound` on purpose, so a client cannot probe for ids. What the client
//! can still separate is: the core is busy, the surface may not read content, the
//! artifact is gone (or is not the one the reference names), and bytes that
//! arrived do not match the digest the reference carries.
use super::super::FeatureError;
use workbench_protocol::errors::{ErrorCode, RetryAdvice};

/// What the connection's data reader reports when the bytes or the control epoch do
/// not match the request (`connection/data.rs`): the only text-only failure that
/// means "the digest does not match". Matched here, in one place.
const DATA_MISMATCH: &str = "data source or control epoch mismatch";
/// The reader's note for a data request that never left a lost connection.
const DATA_DISCONNECTED: &str = "disconnected";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Problem {
    /// The core is busy or restarting. Asked again only when the user asks.
    Busy(Option<RetryAdvice>),
    /// This connection may not read artifact content.
    Denied,
    /// The core no longer offers the artifact (deleted, moved or not this one).
    Gone,
    /// What came back is not the artifact the reference (digest) names.
    DigestMismatch,
    /// Over the compressed, decoded or text limits of the viewer.
    TooLarge(String),
    /// A kind of artifact the viewer does not show.
    Unsupported(String),
    /// The connection ended while it was being read.
    Disconnected,
    Other(String),
}

impl Problem {
    pub(super) fn from_error(error: &FeatureError) -> Self {
        if error.outcome_unknown {
            return Self::Disconnected;
        }
        match error.code {
            Some(ErrorCode::Busy | ErrorCode::Draining | ErrorCode::LimitExceeded) => {
                Self::Busy(error.retry)
            }
            Some(ErrorCode::PermissionDenied | ErrorCode::UnsupportedCapability) => Self::Denied,
            Some(ErrorCode::NotFound) => Self::Gone,
            Some(_) => Self::Other(error.message.clone()),
            None if error.message.contains(DATA_MISMATCH) => Self::DigestMismatch,
            None if error.message == DATA_DISCONNECTED => Self::Disconnected,
            None => Self::Other(error.message.clone()),
        }
    }

    /// A button that asks again is offered, never an automatic retry.
    pub(super) fn can_retry(&self) -> bool {
        match self {
            Self::Busy(advice) => *advice != Some(RetryAdvice::Never),
            Self::Disconnected | Self::Other(_) => true,
            _ => false,
        }
    }

    pub(super) fn line(&self) -> String {
        match self {
            Self::Busy(_) => "The core is busy. Press Retry in a moment.".to_owned(),
            Self::Denied => "This connection is not allowed to read artifact content.".to_owned(),
            Self::Gone => {
                "The artifact was deleted or the reference no longer points to it. Reload the list."
                    .to_owned()
            }
            Self::DigestMismatch => {
                "The received content does not match the reference's digest, so it is not shown."
                    .to_owned()
            }
            Self::TooLarge(why) => format!("Preview unavailable: too large ({why})"),
            Self::Unsupported(kind) => {
                format!("Preview unavailable: unsupported media type ({kind})")
            }
            Self::Disconnected => {
                "Loading stopped because the connection was lost. Open it again after reconnecting."
                    .to_owned()
            }
            Self::Other(message) => format!("Could not load: {message}"),
        }
    }
}
