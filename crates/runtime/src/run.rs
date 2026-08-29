//! AgentRun の識別子・設定、および表層用の DTO を定義します。

use std::fmt;

use event_bus::AgentRunPhase;
use serde::Serialize;

/// ランタイム内の AgentRun を一意に識別する newtype。
///
/// [`Display`](std::fmt::Display) はイベントペイロードの `run_id` 文字列と
/// 同一の `run-{n}` 形式を返す。イベントへの載せ替えはこの形式で行う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct RunId(u64);

impl RunId {
    /// 数値から ID を構築する。
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// 内部の数値表現を返す。
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run-{}", self.0)
    }
}

/// AgentRun の実行設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunConfig {
    /// ユーザー入力を待ち受ける対話モードか。既定は `false` (非対話)。
    pub interactive: bool,
}

/// AgentRun の要約 (一覧表示用 DTO)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentSummary {
    /// 実行 ID。
    pub run_id: RunId,
    /// ロール名識別子。
    pub role_name: String,
    /// 現在の位相。
    pub phase: AgentRunPhase,
}

/// 単一 AgentRun の詳細検査 (検査用 DTO)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentInspection {
    /// 実行 ID。
    pub run_id: RunId,
    /// ロール名識別子。
    pub role_name: String,
    /// 現在の位相。
    pub phase: AgentRunPhase,
    /// 保持するメッセージ数。
    pub message_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 数値 7 と 0 の RunId / When: Display / Then: "run-{n}" 形式 (イベント run_id と同一形式)
    #[test]
    fn run_id_displays_as_run_prefixed() {
        assert_eq!(RunId::new(7).to_string(), "run-7");
        assert_eq!(RunId::new(0).to_string(), "run-0");
    }

    // Given: 数値 42 の RunId / When: JSON 化 / Then: 内部数値として serialize される
    #[test]
    fn run_id_serializes_as_number() {
        let json = serde_json::to_value(RunId::new(42)).expect("serialize RunId");

        assert_eq!(json, serde_json::json!(42));
    }

    // Given: 数値 123 の RunId / When: get / Then: 元の数値を返す
    #[test]
    fn run_id_get_returns_inner_value() {
        assert_eq!(RunId::new(123).get(), 123);
    }

    // Given: RunConfig / When: Default / Then: interactive は false (非対話が既定)
    #[test]
    fn run_config_defaults_to_non_interactive() {
        assert!(!RunConfig::default().interactive);
    }
}
