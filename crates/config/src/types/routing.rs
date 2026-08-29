//! ルーティングに関する設定型を定義します。

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// ルーティング設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RoutingConfig {
    /// ルート名から候補リスト (フォールバック順) へのマップ。
    pub routes: BTreeMap<String, Vec<RouteCandidateConfig>>,
}

/// ルートの候補 1 件分の設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RouteCandidateConfig {
    /// 使用するプロバイダプロファイル名。
    pub profile: String,
    /// モデル ID の上書き指定。省略時はプロファイルの `default_model` を使用する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
    }
}
