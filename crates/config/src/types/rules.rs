//! プロジェクトルール注入に関する設定型を定義します。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// プロジェクトルール注入の設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RulesConfig {
    /// モデルのコンテキストウィンドウ上限トークン数。
    pub context_window_tokens: u64,
    /// 応答生成用に予約するトークン数。
    pub response_headroom_tokens: u64,
    /// 1 回に注入するルール本文の最大バイト数。
    pub max_injection_bytes: u64,
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            context_window_tokens: 200_000,
            response_headroom_tokens: 16_384,
            max_injection_bytes: 65_536,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    // Given: 既定のルール設定 / When: TOML に直列化して再度パースする / Then: 既定値が保存される
    #[test]
    fn defaults_round_trip_through_toml() {
        let config = Config::default();

        let serialized = toml::to_string(&config).expect("既定設定を直列化できる");
        let reparsed: Config = toml::from_str(&serialized).expect("既定設定を再解析できる");

        assert_eq!(reparsed.rules.context_window_tokens, 200_000);
        assert_eq!(reparsed.rules.response_headroom_tokens, 16_384);
        assert_eq!(reparsed.rules.max_injection_bytes, 65_536);
    }

    // Given: rules セクションを持つ TOML / When: Config に解析する / Then: 指定値が読み取られる
    #[test]
    fn parses_rules_section_from_toml() {
        let config: Config = toml::from_str(
            "[rules]\ncontext_window_tokens = 120000\nresponse_headroom_tokens = 8000\nmax_injection_bytes = 4096\n",
        )
        .expect("rules 設定を解析できる");

        assert_eq!(config.rules.context_window_tokens, 120_000);
        assert_eq!(config.rules.response_headroom_tokens, 8_000);
        assert_eq!(config.rules.max_injection_bytes, 4_096);
    }

    // Given: rules セクションに未知フィールドを含む TOML / When: Config に解析する / Then: 拒否される
    #[test]
    fn rejects_unknown_rules_field() {
        let result = toml::from_str::<Config>("[rules]\nunknown = 1\n");

        assert!(result.is_err(), "rules の未知フィールドは拒否される");
    }
}
