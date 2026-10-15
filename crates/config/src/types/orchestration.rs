//! オーケストレーションの境界設定型を定義します。

use std::num::NonZeroU32;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// オーケストレーションの境界設定。
///
/// すべての上限は正の値でなければならない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestrationConfig {
    /// thread goal の独立レビューのラウンド数の上限。上限に達した goal は Blocked になる。
    ///
    /// 新しく作る goal から適用し、作成済みの goal の上限は変えない。
    pub max_review_rounds: NonZeroU32,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            max_review_rounds: NonZeroU32::MIN.saturating_add(2),
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

        assert_eq!(config.max_review_rounds.get(), 3);
    }
}
