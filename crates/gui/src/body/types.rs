//! Names and errors for a body that is stored apart from its event.

use std::fmt;
use std::sync::Arc;

/// First character of a stored body token (a private-use code point). The
/// storage layer reserves it: every string of a tokenized event that starts
/// with it is a token position, whatever follows.
pub const TOKEN_ESCAPE: char = '\u{e000}';

/// The identity of a stored body: the SHA-256 of its UTF-8 bytes and their count.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BodyRef {
    pub hash: [u8; 32],
    pub byte_len: u64,
}

impl From<storage::BodyRef> for BodyRef {
    fn from(reference: storage::BodyRef) -> Self {
        Self {
            hash: reference.hash,
            byte_len: reference.byte_len,
        }
    }
}

impl From<BodyRef> for storage::BodyRef {
    fn from(reference: BodyRef) -> Self {
        Self {
            hash: reference.hash,
            byte_len: reference.byte_len,
        }
    }
}

impl BodyRef {
    #[must_use]
    pub fn hash_hex(&self) -> String {
        storage::BodyRef::from(*self).hash_hex()
    }

    /// Reads a whole string of a tokenized event. `None` means the string is not
    /// at a token position (it does not start with the escape character);
    /// `Some(Err)` means it is at one but is not a token this build understands.
    /// The grammar is storage's.
    #[must_use]
    pub fn parse_token(value: &str) -> Option<Result<Self, BodyError>> {
        if !value.starts_with(TOKEN_ESCAPE) {
            return None;
        }
        Some(
            storage::BodyRef::from_token(value)
                .map(Self::from)
                .ok_or_else(|| {
                    BodyError::new(
                        BodyErrorKind::Corrupt,
                        "a stored body token is malformed or of an unknown version",
                    )
                }),
        )
    }

    /// The token a tokenized event holds for this body (tests build events
    /// with it; storage's own reader is what the app uses).
    #[cfg(test)]
    #[must_use]
    pub fn to_token(&self) -> String {
        format!(
            "\u{e000}evorch-body:v1:{}:{}",
            self.hash_hex(),
            self.byte_len
        )
    }
}

impl fmt::Debug for BodyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BodyRef({}:{})", self.hash_hex(), self.byte_len)
    }
}

/// Why a body could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyErrorKind {
    /// The store holds no such body.
    Missing,
    /// The stored bytes do not match their name, or cannot be decoded.
    Corrupt,
    /// The database was busy or locked.
    Busy,
    /// Reading failed for a reason outside the data.
    Io,
    /// Larger than the absolute limit the store reads.
    TooLarge,
}

impl BodyErrorKind {
    /// Whether trying again can help; only these are retried on their own.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::Busy | Self::Io)
    }

    /// An integrity error: the data itself is wrong or gone.
    #[must_use]
    pub const fn is_integrity(self) -> bool {
        !self.is_transient()
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::Busy => "busy",
            Self::Io => "I/O error",
            Self::TooLarge => "too large",
        }
    }
}

/// A failure to read a body. The reason never contains body text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyError {
    pub kind: BodyErrorKind,
    pub reason: String,
}

impl BodyError {
    #[must_use]
    pub fn new(kind: BodyErrorKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.label(), self.reason)
    }
}

impl std::error::Error for BodyError {}

/// Where a deferred body was found: enough to explain a failure, never the text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BodyOrigin {
    pub run_id: Option<String>,
    pub event_id: Option<i64>,
}

/// A body that is still in the store, and the event it belongs to.
#[derive(Debug, Clone)]
pub struct DeferredBody {
    pub key: BodyRef,
    pub origin: Arc<BodyOrigin>,
}

impl PartialEq for DeferredBody {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl DeferredBody {
    #[must_use]
    pub fn new(key: BodyRef, run_id: Option<&str>, event_id: Option<i64>) -> Self {
        Self {
            key,
            origin: Arc::new(BodyOrigin {
                run_id: run_id.map(str::to_owned),
                event_id,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_round_trips_and_other_strings_are_not_token_positions() {
        let key = BodyRef {
            hash: [0xab; 32],
            byte_len: 4096,
        };
        assert_eq!(BodyRef::parse_token(&key.to_token()).unwrap().unwrap(), key);
        assert!(BodyRef::parse_token("plain text").is_none());
        // The escape character alone marks a token position, so a lookalike fails.
        for bad in [
            "\u{e000}",
            "\u{e000}evorch-body:v1:zz:3",
            "\u{e000}evorch-body:v2:aa:3",
            &format!("\u{e000}evorch-body:v1:{}:03", "a".repeat(64)),
        ] {
            let error = BodyRef::parse_token(bad).unwrap().unwrap_err();
            assert_eq!(error.kind, BodyErrorKind::Corrupt);
        }
    }
}
