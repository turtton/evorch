//! Body tokens: how a long string stored in `content_blobs` is named inside JSON.
//!
//! A token is a whole JSON *string value* of the form
//!
//! ```text
//! U+E000 "evorch-body:v1:" <64 lowercase hex> ":" <UTF-8 byte length>
//! ```
//!
//! The length is decimal without leading zeros (`0` itself is spelled `0`) and must fit
//! an `i64`. Only string **values** are tokenized, never object keys, and resolution works
//! on parsed [`Value`] nodes: a textual search and replace over the serialized JSON would
//! also hit a token-looking substring inside a longer string.
//!
//! # What is tokenized
//!
//! [`needs_token`] is true for a string that is at least [`TOKEN_THRESHOLD_BYTES`] long,
//! and for **any** string starting with U+E000, however short. The second rule is the
//! escape: a stored container never holds a string that starts with U+E000 and is not a
//! token, so a user's text that happens to look like a token (or like a token of a
//! future version) is stored as a blob and comes back unchanged. Resolution therefore
//! treats every string starting with U+E000 as a token position and fails when it is not
//! a valid token.
//!
//! The text a token resolves to is **not** scanned again: it is the original string, and
//! it may itself start with U+E000.
//!
//! Tokenizing and resolving walk the value without recursion, so nesting depth is not
//! limited by the call stack.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::error::BodyUnresolvedReason;

/// First character of every token, and the escape marker (a private-use code point).
pub(crate) const ESCAPE: char = '\u{e000}';
const TOKEN_HEAD: &str = "\u{e000}evorch-body:";
const TOKEN_V1: &str = "v1:";
/// Strings of at least this many UTF-8 bytes are stored as blobs.
pub(crate) const TOKEN_THRESHOLD_BYTES: usize = 2_048;

/// The identity of a stored body: the SHA-256 of its UTF-8 bytes and their count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct BodyToken {
    pub hash: [u8; 32],
    pub byte_len: u64,
}

/// Why a string in a token position could not be read as a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenError {
    /// Does not look like this scheme's token at all.
    Malformed,
    /// Has the token head, but names a version this build does not know.
    UnknownVersion,
}

impl TokenError {
    pub(crate) fn reason(self) -> BodyUnresolvedReason {
        match self {
            Self::Malformed => BodyUnresolvedReason::InvalidToken,
            Self::UnknownVersion => BodyUnresolvedReason::UnknownTokenVersion,
        }
    }
}

impl BodyToken {
    /// The token of `text`.
    pub(crate) fn of(text: &str) -> Self {
        Self {
            hash: Sha256::digest(text.as_bytes()).into(),
            byte_len: text.len() as u64,
        }
    }

    pub(crate) fn hash_hex(&self) -> String {
        hex(&self.hash)
    }

    /// The JSON string value that stands for the body.
    pub(crate) fn to_value_string(self) -> String {
        format!(
            "{TOKEN_HEAD}{TOKEN_V1}{}:{}",
            self.hash_hex(),
            self.byte_len
        )
    }

    /// Reads a whole string value as a token.
    pub(crate) fn parse(value: &str) -> Result<Self, TokenError> {
        let rest = value
            .strip_prefix(TOKEN_HEAD)
            .ok_or(TokenError::Malformed)?;
        let Some(rest) = rest.strip_prefix(TOKEN_V1) else {
            return Err(if is_version_tag(rest) {
                TokenError::UnknownVersion
            } else {
                TokenError::Malformed
            });
        };
        let (hash_text, length_text) = rest.split_once(':').ok_or(TokenError::Malformed)?;
        let hash = parse_hash(hash_text).ok_or(TokenError::Malformed)?;
        if length_text.is_empty()
            || !length_text.bytes().all(|byte| byte.is_ascii_digit())
            || (length_text.len() > 1 && length_text.starts_with('0'))
        {
            return Err(TokenError::Malformed);
        }
        let byte_len: u64 = length_text.parse().map_err(|_| TokenError::Malformed)?;
        if i64::try_from(byte_len).is_err() {
            return Err(TokenError::Malformed);
        }
        Ok(Self { hash, byte_len })
    }
}

/// `v<digits>:` at the start of `rest`.
fn is_version_tag(rest: &str) -> bool {
    let Some(after_v) = rest.strip_prefix('v') else {
        return false;
    };
    let digits = after_v.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && after_v[digits..].starts_with(':')
}

fn parse_hash(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut hash = [0_u8; 32];
    for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        hash[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(hash)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Whether a string value has to be stored as a blob.
pub(crate) fn needs_token(value: &str) -> bool {
    value.len() >= TOKEN_THRESHOLD_BYTES || value.starts_with(ESCAPE)
}

/// Whether any string value of `value` has to be stored as a blob (keys never do).
pub(crate) fn any_needs_token(value: &Value) -> bool {
    let mut stack: Vec<&Value> = vec![value];
    while let Some(node) = stack.pop() {
        match node {
            Value::String(text) if needs_token(text) => return true,
            Value::Array(items) => stack.extend(items.iter()),
            Value::Object(map) => stack.extend(map.values()),
            _ => {}
        }
    }
    false
}

/// Whether a string value sits in a token position when read back.
pub(crate) fn is_token_position(value: &str) -> bool {
    value.starts_with(ESCAPE)
}

/// Visits every string value of `value` in document order, children after their parent
/// is entered, without recursion. The visitor may replace the string; keys are never
/// visited.
fn visit_strings<E>(
    value: &mut Value,
    visitor: &mut impl FnMut(&mut String) -> Result<(), E>,
) -> Result<(), E> {
    let mut stack: Vec<&mut Value> = vec![value];
    while let Some(node) = stack.pop() {
        match node {
            Value::String(text) => visitor(text)?,
            Value::Array(items) => stack.extend(items.iter_mut().rev()),
            Value::Object(map) => {
                // Reverse so the stack pops in document order.
                let children: Vec<&mut Value> = map.values_mut().collect();
                stack.extend(children.into_iter().rev());
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    Ok(())
}

/// Replaces every string value for which [`needs_token`] holds with its token string.
/// `store` saves one body and returns its token; it is called once per such string. On
/// error `value` is partly rewritten and must be discarded.
pub(crate) fn tokenize_in_place(
    value: &mut Value,
    store: &mut impl FnMut(&str) -> Result<BodyToken, StorageError>,
) -> Result<(), StorageError> {
    visit_strings(value, &mut |text: &mut String| {
        if needs_token(text) {
            let token = store(text)?;
            *text = token.to_value_string();
        }
        Ok(())
    })
}

/// Replaces every token position of `value` with the body its token names.
///
/// `load` returns the body of a token (and fails with `BodyUnresolved` when it cannot).
/// A string starting with U+E000 that is not a valid token fails here with the hash
/// unknown; the returned text is not scanned again. On error `value` is partly
/// rewritten and must be discarded. `owner` only labels errors.
pub(crate) fn resolve_in_place(
    value: &mut Value,
    owner: &str,
    load: &mut impl FnMut(&BodyToken) -> Result<String, StorageError>,
) -> Result<(), StorageError> {
    // Bodies loaded so far are inserted as strings that may start with U+E000; the
    // walk below never revisits a replaced node, because `visit_strings` only pushes a
    // node's children, and a string has none.
    visit_strings(value, &mut |text: &mut String| {
        if is_token_position(text) {
            let token = BodyToken::parse(text).map_err(|error| invalid(owner, error))?;
            *text = load(&token)?;
        }
        Ok(())
    })
}

/// The tokens found in `value`, in document order, repeats included. Fails like
/// [`resolve_in_place`] for a malformed token position. Reads without changing `value`.
pub(crate) fn collect_tokens(value: &Value, owner: &str) -> Result<Vec<BodyToken>, StorageError> {
    let mut tokens = Vec::new();
    let mut stack: Vec<&Value> = vec![value];
    while let Some(node) = stack.pop() {
        match node {
            Value::String(text) if is_token_position(text) => {
                tokens.push(BodyToken::parse(text).map_err(|error| invalid(owner, error))?);
            }
            Value::Array(items) => stack.extend(items.iter().rev()),
            Value::Object(map) => stack.extend(map.values().rev()),
            _ => {}
        }
    }
    Ok(tokens)
}

fn invalid(owner: &str, error: TokenError) -> StorageError {
    StorageError::BodyUnresolved {
        owner: owner.to_owned(),
        hash: None,
        reason: error.reason(),
    }
}

#[cfg(test)]
mod tests;
