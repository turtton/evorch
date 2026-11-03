//! Names and errors for a body that is stored apart from its event.

use std::fmt;
use std::sync::Arc;

/// First character of a stored body token (a private-use code point). The
/// storage layer reserves it: every stored string that starts with it is a
/// token position, whatever follows.
pub const TOKEN_ESCAPE: char = '\u{e000}';
const TOKEN_HEAD: &str = "\u{e000}evorch-body:v1:";

/// The identity of a stored body: the SHA-256 of its UTF-8 bytes and their count.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BodyRef {
    pub hash: [u8; 32],
    pub byte_len: u64,
}

impl BodyRef {
    #[must_use]
    pub fn hash_hex(&self) -> String {
        use fmt::Write as _;
        self.hash
            .iter()
            .fold(String::with_capacity(64), |mut out, b| {
                let _ = write!(out, "{b:02x}");
                out
            })
    }

    /// The JSON string value a stored container holds for this body.
    #[must_use]
    pub fn to_token(&self) -> String {
        format!("{TOKEN_HEAD}{}:{}", self.hash_hex(), self.byte_len)
    }

    /// Reads a whole string value. `None` means the string is not at a token
    /// position (it does not start with the escape character); `Some(Err)`
    /// means it is at one but is not a token this build understands.
    #[must_use]
    pub fn parse_token(value: &str) -> Option<Result<Self, BodyError>> {
        if !value.starts_with(TOKEN_ESCAPE) {
            return None;
        }
        Some(Self::parse_known(value).ok_or_else(|| {
            BodyError::new(BodyErrorKind::Corrupt, "a stored body token is malformed")
        }))
    }

    fn parse_known(value: &str) -> Option<Self> {
        let rest = value.strip_prefix(TOKEN_HEAD)?;
        let (hash_text, length_text) = rest.split_once(':')?;
        if hash_text.len() != 64
            || !hash_text
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        if length_text.is_empty()
            || !length_text.bytes().all(|b| b.is_ascii_digit())
            || (length_text.len() > 1 && length_text.starts_with('0'))
        {
            return None;
        }
        let byte_len: u64 = length_text.parse().ok()?;
        if i64::try_from(byte_len).is_err() {
            return None;
        }
        let mut hash = [0u8; 32];
        for (slot, pair) in hash.iter_mut().zip(hash_text.as_bytes().chunks(2)) {
            *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self { hash, byte_len })
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
