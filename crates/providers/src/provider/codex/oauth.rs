//! codex プロバイダの PKCE (RFC 7636) プリミティブを提供します。

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::error::ProviderError;

/// PKCE の challenge メソッド。
pub const PKCE_CHALLENGE_METHOD: &str = "S256";

/// verifier の元となる乱数バイト数。base64url (無パディング) で 86 文字になる。
const VERIFIER_RANDOM_BYTES: usize = 64;

/// PKCE verifier とその S256 challenge のペア。
///
/// [`fmt::Debug`] は手動実装で verifier を `<redacted>` に置き換えて出力する
/// (challenge は認可 URL に平文で送信される公開値のためそのまま出力する)。
pub struct PkcePair {
    /// base64url (無パディング) エンコード済み verifier (86 文字)。
    pub verifier: String,
    /// verifier の SHA-256 を base64url (無パディング) エンコードした challenge。
    pub challenge: String,
}

impl PkcePair {
    /// 暗号学的に安全な乱数から PKCE ペアを生成する。
    ///
    /// # Errors
    /// 乱数生成に失敗した場合 [`ProviderError::Request`] を返す。
    pub fn generate() -> Result<Self, ProviderError> {
        let mut bytes = [0u8; VERIFIER_RANDOM_BYTES];
        getrandom::fill(&mut bytes).map_err(|error| {
            ProviderError::Request(format!("PKCE verifier の乱数生成に失敗しました: {error}"))
        })?;
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = Self::challenge_for(&verifier);
        Ok(Self {
            verifier,
            challenge,
        })
    }

    /// verifier から S256 challenge を計算する。
    pub fn challenge_for(verifier: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }
}

impl fmt::Debug for PkcePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PkcePair")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC7636_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC7636_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    // Given: RFC 7636 付録 B の verifier / When: challenge_for / Then: 既知の challenge と一致する
    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        assert_eq!(PkcePair::challenge_for(RFC7636_VERIFIER), RFC7636_CHALLENGE);
    }

    // Given: generate() の返り値 / When: 形状検査 / Then: verifier は 86 文字の URL セーフ文字のみで challenge と整合する
    #[test]
    fn pkce_generated_verifier_shape() {
        let pair = PkcePair::generate().expect("generate は成功する");

        assert_eq!(pair.verifier.len(), 86);
        assert!(
            pair.verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(pair.challenge, PkcePair::challenge_for(&pair.verifier));
    }

    // Given: verifier を含む PkcePair / When: Debug 出力 / Then: verifier は現れず <redacted> に置き換わる
    #[test]
    fn pkce_debug_redacts_verifier() {
        let pair = PkcePair {
            verifier: RFC7636_VERIFIER.to_string(),
            challenge: RFC7636_CHALLENGE.to_string(),
        };

        let rendered = format!("{pair:?}");

        assert!(!rendered.contains(RFC7636_VERIFIER));
        assert!(rendered.contains("<redacted>"));
    }
}
