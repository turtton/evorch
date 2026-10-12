//! ルーティングに関する設定型を定義します。

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// ルーティング設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    /// ルート名から候補リスト (フォールバック順) へのマップ。
    pub routes: BTreeMap<String, Vec<RouteCandidateConfig>>,
    /// 失敗したプロファイル・モデルを候補から外す期間。
    #[serde(skip_serializing_if = "CooldownConfig::is_default")]
    pub cooldown: CooldownConfig,
}

/// 失敗後のクールダウン期間 (ADR 0030)。
///
/// Retry-After などプロバイダが期間を示した場合はそちらを優先する。認証エラーは
/// 期間を持たず、runtime の再構成 (設定の再読み込み) まで続く。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CooldownConfig {
    /// 支払い・利用上限による失敗でプロファイル全体を止める秒数。
    pub quota_secs: u64,
    /// レート制限による失敗で (プロファイル, モデル) を止める秒数。
    pub rate_limit_secs: u64,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            quota_secs: 1_800,
            rate_limit_secs: 30,
        }
    }
}

impl CooldownConfig {
    /// 既定値と等しいか (保存時に省略できるか)。
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// ルートの候補 1 件分の設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RouteCandidateConfig {
    /// 使用するプロバイダプロファイル名。
    pub profile: String,
    /// モデル ID の上書き指定。省略時はプロファイルの `default_model` を使用する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// この候補で送る推論強度。省略時はプロバイダ既定。モデルごとに有効な値が異なるため自由形式の文字列とする。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: model キーあり・なしの候補 TOML / When: それぞれ解析する
    // Then: 省略時は None、指定時は Some になる
    #[test]
    fn route_candidate_optional_model_deserializes() {
        let without_model: RouteCandidateConfig =
            toml::from_str(r#"profile = "anthropic-main""#).expect("model 省略の候補を解析できる");
        assert_eq!(without_model.profile, "anthropic-main");
        assert_eq!(without_model.model, None);

        let with_model: RouteCandidateConfig = toml::from_str(
            r#"
profile = "anthropic-main"
model = "claude-opus-4-1"
"#,
        )
        .expect("model 指定の候補を解析できる");
        assert_eq!(with_model.profile, "anthropic-main");
        assert_eq!(with_model.model.as_deref(), Some("claude-opus-4-1"));
        assert_eq!(with_model.reasoning_effort, None);

        let with_effort: RouteCandidateConfig = toml::from_str(
            r#"
profile = "openai-main"
model = "gpt-5"
reasoning_effort = "high"
"#,
        )
        .expect("reasoning_effort 指定の候補を解析できる");
        assert_eq!(with_effort.reasoning_effort.as_deref(), Some("high"));
    }
}
