//! Versioned, authenticated `thread_search` continuation tokens.
//!
//! A token binds the caller (run, thread and project), the normalized
//! search conditions and the position, under an HMAC-SHA256 keyed by a
//! per-process random key. Tampering, a different caller and a changed query
//! are all detected before any record is read; the position itself is still
//! re-authorized on every call. Tokens do not survive a restart, and Phase A
//! (`v1:`) tokens are not accepted: they carried no conditions or MAC.

use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const PREFIX: &str = "ts2.";
const VERSION: u32 = 2;
const BLOCK: usize = 64;
const MAC_BYTES: usize = 16;

/// Where the next page starts: threads after `thread` (a storage position
/// token), or, with `resume`, first the rest of that thread's conversation.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(super) struct Position {
    pub(super) thread: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) resume: Option<Resume>,
}

/// Continue at block `from_block` (inclusive) of `run`, which must still be
/// indexed at `revision`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(super) struct Resume {
    pub(super) run: String,
    pub(super) from_block: i64,
    pub(super) revision: u64,
    /// Fixed-size digest of this thread's run set and index revisions. Metadata
    /// ownership must remain valid even when its canonical position is on a later page.
    pub(super) thread_revision: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    v: u32,
    caller: String,
    query: String,
    #[serde(flatten)]
    position: Position,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CursorError {
    /// Malformed, forged, another caller's, or from an older format.
    Invalid,
    /// Authentic, but issued for different search conditions.
    Mismatch,
}

/// Short hex digest binding free-form fields without echoing them.
pub(super) fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn encode(caller: &str, query: &str, position: &Position) -> String {
    let payload = serde_json::to_vec(&Payload {
        v: VERSION,
        caller: caller.to_owned(),
        query: query.to_owned(),
        position: position.clone(),
    })
    .expect("cursor payload serializes");
    format!(
        "{PREFIX}{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(&mac(&payload)[..MAC_BYTES])
    )
}

pub(super) fn decode(token: &str, caller: &str, query: &str) -> Result<Position, CursorError> {
    let (payload, tag) = token
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once('.'))
        .ok_or(CursorError::Invalid)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| CursorError::Invalid)?;
    let tag = URL_SAFE_NO_PAD
        .decode(tag)
        .map_err(|_| CursorError::Invalid)?;
    if !constant_time_eq(&tag, &mac(&payload)[..MAC_BYTES]) {
        return Err(CursorError::Invalid);
    }
    let payload: Payload = serde_json::from_slice(&payload).map_err(|_| CursorError::Invalid)?;
    if payload.v != VERSION || payload.caller != caller {
        return Err(CursorError::Invalid);
    }
    if payload.query != query {
        return Err(CursorError::Mismatch);
    }
    Ok(payload.position)
}

fn key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = [0; 32];
        getrandom::fill(&mut key).expect("the OS random source is available");
        key
    })
}

fn mac(message: &[u8]) -> [u8; 32] {
    hmac(key(), message)
}

/// HMAC-SHA256 (RFC 2104) for keys of at most one block.
fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut padded = [0_u8; BLOCK];
    padded[..key.len()].copy_from_slice(key);
    let inner: Vec<u8> = padded.iter().map(|byte| byte ^ 0x36).collect();
    let outer: Vec<u8> = padded.iter().map(|byte| byte ^ 0x5c).collect();
    let inner = Sha256::new()
        .chain_update(&inner)
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(&outer)
        .chain_update(inner)
        .finalize()
        .into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position() -> Position {
        Position {
            thread: "v1:5:3".into(),
            resume: Some(Resume {
                run: "run-7".into(),
                from_block: 42,
                revision: 3,
                thread_revision: "thread-revisions".into(),
            }),
        }
    }

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        let tag: String = hmac(b"Jefe", b"what do ya want for nothing?")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            tag,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn tokens_round_trip_and_reject_tampering_callers_queries_and_versions() {
        let token = encode("caller", "query", &position());
        assert_eq!(decode(&token, "caller", "query"), Ok(position()));
        assert_eq!(decode(&token, "other", "query"), Err(CursorError::Invalid));
        assert_eq!(
            decode(&token, "caller", "changed"),
            Err(CursorError::Mismatch)
        );
        for index in PREFIX.len()..token.len() {
            let mut bytes = token.clone().into_bytes();
            bytes[index] = if bytes[index] == b'A' { b'B' } else { b'A' };
            let forged = String::from_utf8(bytes).unwrap();
            if forged != token {
                assert_eq!(
                    decode(&forged, "caller", "query"),
                    Err(CursorError::Invalid),
                    "{forged}"
                );
            }
        }
        for legacy in ["v1:5:3", "", "ts2.", "ts2.e30.", "ts1.x.y"] {
            assert_eq!(decode(legacy, "caller", "query"), Err(CursorError::Invalid));
        }
    }
}
