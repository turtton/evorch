//! provider 認証情報を扱います。
//!
//! ベース URL はクライアントのコンストラクタ設定であり、認証情報には含まれない。
//! 認証情報はリクエストごとに注入され、クライアントの状態として永続保存されない。

use std::fmt;

/// API キー認証情報。
///
/// [`fmt::Debug`] は手動実装でキーを `<redacted>` に置き換えて出力するため、
/// ログに表示しても生キーが漏れない。
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderAuth {
    /// 生の API キー。Debug 出力では必ず隠蔽される。
    pub api_key: String,
}

impl ProviderAuth {
    /// 生キーから認証情報を生成する。
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }
}

impl fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderAuth")
            .field("api_key", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 生キーを持つ ProviderAuth / When: Debug 出力 / Then: 生キーは現れず <redacted> に置き換わる
    #[test]
    fn debug_output_redacts_api_key() {
        let auth = ProviderAuth::new("sk-secret-123");

        let rendered = format!("{auth:?}");

        assert!(!rendered.contains("sk-secret-123"));
        assert_eq!(rendered, r#"ProviderAuth { api_key: "<redacted>" }"#);
    }

    // Given: &str から生成した ProviderAuth / When: api_key を読み取る / Then: 元のキーが保持される
    #[test]
    fn new_stores_raw_api_key() {
        let auth = ProviderAuth::new("sk-abc");

        assert_eq!(auth.api_key, "sk-abc");
    }
}
