//! codex プロバイダのトークン管理プリミティブを提供します。

use std::fmt;
use std::sync::{Mutex, PoisonError};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;

/// トークンを先行更新する猶予時間 (秒)。
const REFRESH_WINDOW_SECS: u64 = 300;

/// codex の認証トークン一式。単一の JSON オブジェクトとして永続化される。
///
/// [`fmt::Debug`] は手動実装で全フィールドを `<redacted>` に置き換えて出力するため、
/// ログに表示してもトークン実体が漏れない。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenBundle {
    /// API 呼び出しに使用するアクセストークン。
    pub access_token: String,
    /// 更新フローで使用するリフレッシュトークン。
    pub refresh_token: String,
    /// アカウント識別クレームを含む ID トークン。
    pub id_token: String,
}

impl fmt::Debug for TokenBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenBundle")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("id_token", &"<redacted>")
            .finish()
    }
}

/// ID トークン (JWT) から抽出した codex 必須クレーム。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexJwtClaims {
    /// 有効期限 (UNIX エポック秒)。
    pub exp: u64,
    /// ChatGPT アカウント ID。
    pub chatgpt_account_id: String,
}

/// ID トークン payload の生クレーム。serde による抽出専用。
#[derive(Deserialize)]
struct RawJwtClaims {
    exp: u64,
    #[serde(rename = "https://api.openai.com/auth")]
    auth: RawAuthClaims,
}

#[derive(Deserialize)]
struct RawAuthClaims {
    chatgpt_account_id: String,
}

/// ID トークン (JWT) の payload から codex 必須クレームを抽出する。
///
/// 署名は検証しない。payload はパディングあり / なし両方の base64url を受け付ける。
///
/// # Errors
/// セグメント数が 3 でない場合、base64url 復号に失敗した場合、payload の JSON 解析に
/// 失敗した場合、および `exp` またはネストした `chatgpt_account_id` が欠落している場合に
/// [`ProviderError::InvalidJson`] を返す。
pub fn parse_jwt_claims(id_token: &str) -> Result<CodexJwtClaims, ProviderError> {
    let segments: Vec<&str> = id_token.split('.').collect();
    let payload = match segments.as_slice() {
        [_, payload, _] => payload,
        _ => {
            return Err(ProviderError::InvalidJson {
                detail: format!(
                    "id_token は 3 セグメントである必要があります (実際: {} 個)",
                    segments.len()
                ),
            });
        }
    };

    let bytes = decode_payload(payload)?;
    let raw: RawJwtClaims =
        serde_json::from_slice(&bytes).map_err(|error| ProviderError::InvalidJson {
            detail: format!("id_token payload の解析に失敗しました: {error}"),
        })?;

    Ok(CodexJwtClaims {
        exp: raw.exp,
        chatgpt_account_id: raw.auth.chatgpt_account_id,
    })
}

/// payload セグメントを base64url 復号する。
///
/// 末尾の `=` を取り除いてから無パディングエンジンで復号するため、
/// パディングあり / なしの両方を受け付ける。
fn decode_payload(segment: &str) -> Result<Vec<u8>, ProviderError> {
    URL_SAFE_NO_PAD
        .decode(segment.trim_end_matches('='))
        .map_err(|error| ProviderError::InvalidJson {
            detail: format!("id_token payload の base64url 復号に失敗しました: {error}"),
        })
}

/// `exp` が更新猶予時間内 (または既に失効) かを判定する。
///
/// `now >= exp - 300` で true。`exp <= now` の場合は飽和減算により true になる。
pub const fn needs_refresh(now_unix: u64, exp: u64) -> bool {
    exp.saturating_sub(now_unix) <= REFRESH_WINDOW_SECS
}

/// codex トークン一式の永続化抽象。
///
/// 実運用では routing 側の `CredentialStore` アダプタが OS キーストアへ委譲する
/// 実装を提供する想定。
pub trait CodexTokenStore: Send + Sync {
    /// 保存済みトークンを返す。未保存なら `None`。
    ///
    /// # Errors
    /// 読み出しに失敗した場合 [`ProviderError`] を返す。
    fn load(&self) -> Result<Option<TokenBundle>, ProviderError>;

    /// トークン一式を保存する。
    ///
    /// # Errors
    /// 書き込みに失敗した場合 [`ProviderError`] を返す。
    fn save(&self, bundle: &TokenBundle) -> Result<(), ProviderError>;
}

/// テスト・組み込み用のメモリ内トークンストア。
///
/// 永続化は行わない。実運用では routing 側の `CredentialStore` アダプタを使用する。
#[derive(Default)]
pub struct InMemoryTokenStore {
    bundle: Mutex<Option<TokenBundle>>,
}

impl InMemoryTokenStore {
    /// 空のストアを生成する。
    pub fn new() -> Self {
        Self::default()
    }
}

impl CodexTokenStore for InMemoryTokenStore {
    fn load(&self) -> Result<Option<TokenBundle>, ProviderError> {
        Ok(self
            .bundle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    fn save(&self, bundle: &TokenBundle) -> Result<(), ProviderError> {
        *self.bundle.lock().unwrap_or_else(PoisonError::into_inner) = Some(bundle.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE;

    fn sample_bundle() -> TokenBundle {
        TokenBundle {
            access_token: "access-secret".to_string(),
            refresh_token: "refresh-secret".to_string(),
            id_token: "id-secret".to_string(),
        }
    }

    fn base64url_nopad(input: &str) -> String {
        URL_SAFE_NO_PAD.encode(input.as_bytes())
    }

    fn jwt_with_payload(payload: &str) -> String {
        format!(
            "{}.{}.sig-garbage",
            base64url_nopad(r#"{"alg":"HS256","typ":"JWT"}"#),
            base64url_nopad(payload)
        )
    }

    // Given: TokenBundle / When: JSON 文字列へ出力して復元 / Then: 3 キーの単一オブジェクトとして等価に往復する
    #[test]
    fn token_bundle_json_roundtrip() {
        let bundle = sample_bundle();

        let json = serde_json::to_string(&bundle).expect("serialize は成功する");
        let parsed: TokenBundle = serde_json::from_str(&json).expect("deserialize は成功する");

        assert_eq!(parsed, bundle);

        let object = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json)
            .expect("JSON はオブジェクト");
        assert_eq!(object.len(), 3);
        assert!(object.contains_key("access_token"));
        assert!(object.contains_key("refresh_token"));
        assert!(object.contains_key("id_token"));
    }

    // Given: exp とネストした account id を持つ ID トークン / When: parse_jwt_claims / Then: 両クレームが抽出される
    #[test]
    fn jwt_payload_parse_extracts_exp_and_account_id() {
        let id_token = jwt_with_payload(
            r#"{"exp":1893456000,"https://api.openai.com/auth":{"chatgpt_account_id":"acc-123"}}"#,
        );

        let claims = parse_jwt_claims(&id_token).expect("parse_jwt_claims は成功する");

        assert_eq!(claims.exp, 1893456000);
        assert_eq!(claims.chatgpt_account_id, "acc-123");
    }

    // Given: パディング付き payload を含む ID トークン / When: parse_jwt_claims / Then: 復号に成功する
    #[test]
    fn jwt_payload_parse_accepts_padded_payload() {
        let payload =
            r#"{"exp":1893456000,"https://api.openai.com/auth":{"chatgpt_account_id":"acc-pad"}}"#;
        let padded = URL_SAFE.encode(payload.as_bytes());
        let id_token = format!(
            "{}.{}.sig-garbage",
            base64url_nopad(r#"{"alg":"HS256"}"#),
            padded
        );

        let claims = parse_jwt_claims(&id_token).expect("parse_jwt_claims は成功する");

        assert_eq!(claims.chatgpt_account_id, "acc-pad");
    }

    // Given: 各種不正入力 / When: parse_jwt_claims / Then: すべて InvalidJson
    #[test]
    fn jwt_payload_parse_rejects_malformed() {
        let missing_exp =
            jwt_with_payload(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-123"}}"#);
        let missing_nested = jwt_with_payload(r#"{"exp":1893456000}"#);
        let invalid_base64 = format!("{}.!!!!.sig-garbage", base64url_nopad(r#"{"alg":"HS256"}"#));

        for id_token in [
            "garbage".to_string(),
            "aaa.bbb".to_string(),
            invalid_base64,
            missing_exp,
            missing_nested,
        ] {
            let err = parse_jwt_claims(&id_token)
                .expect_err("不正な ID トークンは parse_jwt_claims に失敗する");

            assert!(
                matches!(err, ProviderError::InvalidJson { .. }),
                "actual: {err:?} for {id_token}"
            );
        }
    }

    // Given: exp との差がちょうど 300 秒 / 301 秒 / 失効済み / When: needs_refresh / Then: true / false / true
    #[test]
    fn expiry_window_boundary() {
        assert!(needs_refresh(1000, 1300));
        assert!(!needs_refresh(1000, 1301));
        assert!(needs_refresh(1000, 999));
    }

    // Given: 空の InMemoryTokenStore → 保存 → 再読み出し / When: load / Then: None → save → Some(等価)
    #[test]
    fn in_memory_token_store_roundtrip() {
        let store = InMemoryTokenStore::new();

        assert_eq!(store.load().expect("load は成功する"), None);

        let bundle = sample_bundle();
        store.save(&bundle).expect("save は成功する");

        assert_eq!(store.load().expect("load は成功する"), Some(bundle));
    }

    // Given: TokenBundle / When: Debug 出力 / Then: トークン実体は現れず <redacted> が 3 回出現する
    #[test]
    fn token_bundle_debug_redacted() {
        let rendered = format!("{:?}", sample_bundle());

        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains("refresh-secret"));
        assert!(!rendered.contains("id-secret"));
        assert_eq!(rendered.matches("<redacted>").count(), 3);
    }
}
