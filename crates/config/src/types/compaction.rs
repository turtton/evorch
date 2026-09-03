//! コンテキスト圧縮に関する設定型を定義します。

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// コンテキスト圧縮の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// コンテキスト圧縮の有効フラグ。
    pub enabled: bool,
    /// 圧縮を開始するコンテキスト使用率の閾値。
    pub threshold: f64,
    /// 既定のコンテキストウィンドウ上限トークン数。
    pub context_window_tokens: u64,
    /// モデル ID ごとのコンテキストウィンドウ上限トークン数。
    pub model_overrides: BTreeMap<String, u64>,
    /// 圧縮せずに保持する直近トークン数。
    pub keep_recent_tokens: u64,
    /// 圧縮後に再圧縮を抑制するターン数。
    pub cooldown_turns: u32,
    /// 要約本文の最大バイト数。
    pub max_summary_bytes: u64,
    /// 要約に使用する方式。
    pub summarizer: SummarizerKind,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.75,
            context_window_tokens: 200_000,
            model_overrides: BTreeMap::new(),
            keep_recent_tokens: 20_000,
            cooldown_turns: 1,
            max_summary_bytes: 16_384,
            summarizer: SummarizerKind::Model,
        }
    }
}

/// コンテキスト要約の方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SummarizerKind {
    /// モデルで要約する。
    #[default]
    Model,
    /// 構造情報から要約する。
    Structural,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{CompactionConfig, Config, SummarizerKind};

    // Given: 既定の Config / When: compaction 設定を参照する / Then: 仕様どおりの既定値である
    #[test]
    fn defaults_use_documented_compaction_values() {
        let config = Config::default();

        assert!(config.compaction.enabled);
        assert_eq!(config.compaction.threshold, 0.75);
        assert_eq!(config.compaction.context_window_tokens, 200_000);
        assert!(config.compaction.model_overrides.is_empty());
        assert_eq!(config.compaction.keep_recent_tokens, 20_000);
        assert_eq!(config.compaction.cooldown_turns, 1);
        assert_eq!(config.compaction.max_summary_bytes, 16_384);
        assert_eq!(config.compaction.summarizer, SummarizerKind::Model);
    }

    // Given: 全項目を指定した compaction テーブル / When: Config にパースする
    // Then: 指定値がすべて読み取られる
    #[test]
    fn parses_populated_compaction_section_from_toml() {
        let config: Config = toml::from_str(
            r#"
[compaction]
enabled = false
threshold = 0.9
context_window_tokens = 128000
keep_recent_tokens = 12000
cooldown_turns = 3
max_summary_bytes = 8192
summarizer = "structural"

[compaction.model_overrides]
"gpt-5.2" = 150000
"claude-sonnet-4-5" = 180000
"#,
        )
        .expect("compaction 設定をパースできる");

        assert!(!config.compaction.enabled);
        assert_eq!(config.compaction.threshold, 0.9);
        assert_eq!(config.compaction.context_window_tokens, 128_000);
        assert_eq!(config.compaction.keep_recent_tokens, 12_000);
        assert_eq!(config.compaction.cooldown_turns, 3);
        assert_eq!(config.compaction.max_summary_bytes, 8_192);
        assert_eq!(config.compaction.summarizer, SummarizerKind::Structural);
        assert_eq!(config.compaction.model_overrides["gpt-5.2"], 150_000);
        assert_eq!(
            config.compaction.model_overrides["claude-sonnet-4-5"],
            180_000
        );
    }

    // Given: compaction セクションに未知キーを含む TOML / When: Config にパースする
    // Then: 拒否される
    #[test]
    fn rejects_unknown_compaction_field() {
        let result = toml::from_str::<Config>("[compaction]\nunknown = 1\n");

        assert!(result.is_err(), "compaction の未知フィールドは拒否される");
    }

    // Given: 既定値と異なる compaction 設定 / When: TOML に直列化して再パースする
    // Then: 設定値が完全に保持される
    #[test]
    fn compaction_config_round_trips_through_toml() {
        let config = CompactionConfig {
            enabled: false,
            threshold: 0.8,
            context_window_tokens: 100_000,
            model_overrides: BTreeMap::from([(String::from("model-a"), 90_000)]),
            keep_recent_tokens: 10_000,
            cooldown_turns: 2,
            max_summary_bytes: 4_096,
            summarizer: SummarizerKind::Structural,
        };

        let serialized = toml::to_string(&config).expect("compaction 設定を直列化できる");
        let reparsed: CompactionConfig =
            toml::from_str(&serialized).expect("compaction 設定を再パースできる");

        assert_eq!(reparsed, config);
    }

    // Given: 複数モデルのコンテキスト上限 / When: model_overrides をパースする
    // Then: モデル ID ごとの上限マップになる
    #[test]
    fn parses_compaction_model_overrides_map() {
        let config: CompactionConfig = toml::from_str(
            r#"
context_window_tokens = 200000
[model_overrides]
"model-a" = 100000
"model-b" = 300000
"#,
        )
        .expect("model_overrides をパースできる");

        assert_eq!(config.model_overrides.len(), 2);
        assert_eq!(config.model_overrides["model-a"], 100_000);
        assert_eq!(config.model_overrides["model-b"], 300_000);
    }
}
