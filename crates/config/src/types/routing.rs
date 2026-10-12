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
    /// ロールのチェーンをフォールバックするときの待機設定。
    #[serde(skip_serializing_if = "FallbackConfig::is_default")]
    pub fallback: FallbackConfig,
}

/// チェーンのフォールバック設定 (ADR 0030)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackConfig {
    /// 一時的な失敗でプールが尽きたとき、次のプールへ進む前に Retry-After を待つ上限秒数。
    /// これより長い Retry-After は待たずに次のプールへ進む。0 なら待たない。
    pub max_retry_after_wait_secs: u64,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            max_retry_after_wait_secs: 30,
        }
    }
}

impl FallbackConfig {
    /// 既定値と等しいか (保存時に省略できるか)。
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
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

/// ルート (同じモデルへの経路プール) の候補 1 件分の設定。
///
/// 推論強度はロールのチェーンの要素 ([`crate::ChainEntryConfig`]) で指定する。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RouteCandidateConfig {
    /// 使用するプロバイダプロファイル名。
    pub profile: String,
    /// モデル ID の上書き指定。省略時はプロファイルの `default_model` を使用する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 同じモデルの自動グルーピングで追加された候補か。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
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
        assert!(!with_model.auto);

        let auto: RouteCandidateConfig = toml::from_str(
            r#"
profile = "openai-main"
model = "gpt-5"
auto = true
"#,
        )
        .expect("auto 指定の候補を解析できる");
        assert!(auto.auto);
        assert!(
            toml::from_str::<RouteCandidateConfig>(
                "profile = \"p\"\nreasoning_effort = \"high\"\n"
            )
            .is_err(),
            "推論強度は候補に置けない"
        );
    }
}
