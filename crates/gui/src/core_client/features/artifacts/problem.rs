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
            Self::Busy(_) => {
                "コアがビジーです。しばらくしてから「再試行」を押してください。".to_owned()
            }
            Self::Denied => "この接続では成果物の内容を読む権限がありません。".to_owned(),
            Self::Gone => {
                "成果物は削除されたか、参照が指すものではなくなりました（一覧を再読み込みしてください）。"
                    .to_owned()
            }
            Self::DigestMismatch => {
                "受け取った内容が参照のダイジェストと一致しません。内容は表示しません。".to_owned()
            }
            Self::TooLarge(why) => format!("大きすぎて表示できません: {why}"),
            Self::Unsupported(kind) => format!("この種類（{kind}）は表示できません。"),
            Self::Disconnected => {
                "接続が切れたため読み込みを中止しました。再接続後に開き直してください。".to_owned()
            }
            Self::Other(message) => format!("読み込めませんでした: {message}"),
        }
    }
}
