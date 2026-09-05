//! オーケストレーションループの境界設定型を定義します。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// オーケストレーションループの境界設定。
///
/// すべての上限は正の値でなければならず、既定値は issue #73 の
/// bounded 契約 (AC5) で固定される。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestrationConfig {
    /// レビュー (request-update → repair) ラウンド数の上限。
    pub max_review_rounds: u32,
    /// stall した run への連続 nudge 回数の上限。
    pub max_nudges: u32,
    /// 進捗なしと判定するまでの無活動秒数。
    pub stall_after_secs: u64,
    /// stall 観測サンプラの実行間隔秒数。
    pub stall_check_secs: u64,
    /// ツール in-flight 中に stall 窓へ掛ける倍率。
    pub in_flight_tool_multiplier: u32,
    /// stall と判定する連続ツールエラー回数のしきい値。
    pub repeated_error_threshold: u32,
    /// idle 継続ディスパッチ回数の上限。
    pub max_continuations: u32,
    /// CI 状態のポーリング間隔秒数。
    pub ci_poll_secs: u64,
    /// CI 完了待機のタイムアウト秒数。
    pub ci_timeout_secs: u64,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            max_review_rounds: 3,
            max_nudges: 2,
            stall_after_secs: 600,
            stall_check_secs: 30,
            in_flight_tool_multiplier: 3,
            repeated_error_threshold: 5,
            max_continuations: 8,
            ci_poll_secs: 30,
            ci_timeout_secs: 3600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OrchestrationConfig;

    // Given: 既定の OrchestrationConfig
    // When: 既定値を参照する
    // Then: 仕様 (issue #73 W0) どおりの値であり、すべての上限が正である
    #[test]
    fn orchestration_defaults_are_bounded() {
        let config = OrchestrationConfig::default();

        assert_eq!(config.max_review_rounds, 3);
        assert_eq!(config.max_nudges, 2);
        assert_eq!(config.stall_after_secs, 600);
        assert_eq!(config.stall_check_secs, 30);
        assert_eq!(config.in_flight_tool_multiplier, 3);
        assert_eq!(config.repeated_error_threshold, 5);
        assert_eq!(config.max_continuations, 8);
        assert_eq!(config.ci_poll_secs, 30);
        assert_eq!(config.ci_timeout_secs, 3600);

        assert!(config.max_review_rounds > 0);
        assert!(config.max_nudges > 0);
        assert!(config.stall_after_secs > 0);
        assert!(config.stall_check_secs > 0);
        assert!(config.in_flight_tool_multiplier > 0);
        assert!(config.repeated_error_threshold > 0);
        assert!(config.max_continuations > 0);
        assert!(config.ci_poll_secs > 0);
        assert!(config.ci_timeout_secs > 0);
    }
}
