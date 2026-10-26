//! Offline Codex (ChatGPT subscription) credential fixtures.
//!
//! The ID token is an unsigned JWT carrying only `exp` and the ChatGPT account id
//! claim that the Codex client reads; it is meaningless outside these fixtures.

use serde_json::{Value, json};

/// Unix time 2100-01-01T00:00:00Z: a token expiry no test run reaches.
pub const FAR_FUTURE_EXP: u64 = 4_102_444_800;

/// An unsigned ID token whose payload names `account_id` and expires at `exp_unix`.
pub fn id_token(account_id: &str, exp_unix: u64) -> String {
    let payload = json!({
        "exp": exp_unix,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id},
    });
    format!(
        "e30.{}.signature",
        base64url(payload.to_string().as_bytes())
    )
}

/// The token bundle shape stored in the credential store and returned by
/// `POST /oauth/token` (see [`crate::StreamingMockOpenAi::set_oauth_token_response`]).
pub fn token_bundle(
    access_token: &str,
    refresh_token: &str,
    account_id: &str,
    exp_unix: u64,
) -> Value {
    json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "id_token": id_token(account_id, exp_unix),
    })
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| {
                value | (u32::from(*byte) << (16 - 8 * index))
            });
        for position in 0..=chunk.len() {
            let sextet = (value >> (18 - 6 * position)) & 0x3f;
            encoded.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_token_matches_the_established_fixture_encoding() {
        assert_eq!(
            id_token("acc", FAR_FUTURE_EXP),
            "e30.eyJleHAiOjQxMDI0NDQ4MDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2MifX0.signature"
        );
        for (input, expected) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("\u{ff}\u{fe}", "w7_Dvg"),
        ] {
            assert_eq!(base64url(input.as_bytes()), expected, "{input:?}");
        }
    }
}
