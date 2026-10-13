//! オーケストレーションの境界設定型を定義します。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// オーケストレーションの境界設定。
///
/// すべての上限は正の値でなければならない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestrationConfig {
    /// レビュー (request-update → repair) ラウンド数の上限。
    pub max_review_rounds: u32,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            max_review_rounds: 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OrchestrationConfig;

    // Given: 既定の OrchestrationConfig
    // When: 既定値を参照する
    // Then: レビューラウンドの上限が正の既定値である
    #[test]
    fn orchestration_defaults_are_bounded() {
        let config = OrchestrationConfig::default();

        assert_eq!(config.max_review_rounds, 3);
        assert!(config.max_review_rounds > 0);
    }
}
