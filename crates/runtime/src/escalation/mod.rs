//! Direct run から Orchestrator root run への昇格データを定義する。
//!
//! Direct run は終端状態へ遷移してから引き継ぎ、ADR 0022 に従って新規 root run として
//! Orchestrator を起動する。workspace は所有権 move により排他的に譲渡する。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub(crate) mod detector;
pub(crate) mod prompt;

/// Direct run から Orchestrator root run へ渡す固定スキーマの引継ぎメモ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct EscalationMemo {
    /// 昇格元 Direct run の識別子。
    pub source_run_id: crate::run::RunId,
    /// 昇格元 run が受け取った原要求。
    pub original_request: String,
    /// 昇格までに得られた調査結果。
    pub findings: Vec<String>,
    /// 昇格元 run が変更または調査したファイル。
    pub files_touched: Vec<PathBuf>,
    /// Direct run 単独では解消できなかった阻害要因。
    pub blockers: Vec<String>,
    /// dirty files 一覧と要約を含む workspace のテキスト状態。
    pub workspace_state: String,
    /// Direct run を昇格させた理由。
    pub escalation_reason: String,
    /// Orchestrator が次に取るべき推奨アクション。
    pub suggested_next: String,
}

/// Direct run を Orchestrator へ昇格する検出閾値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscalationSettings {
    /// 連続する編集失敗数の閾値。
    pub consecutive_edit_failures: u32,
    /// 同一ファイルを書き換えた回数の閾値。
    pub same_file_rewrites: u32,
    /// エスカレーション前に許容するツール呼び出し数の閾値。
    pub tool_call_threshold: u32,
}

impl Default for EscalationSettings {
    fn default() -> Self {
        Self {
            consecutive_edit_failures: 3,
            same_file_rewrites: 5,
            tool_call_threshold: 200,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{EscalationMemo, EscalationSettings};
    use crate::RunId;

    // Given: 既定のエスカレーション設定 / When: Default / Then: 検出閾値は 3 / 5 / 200 となる
    #[test]
    fn escalation_settings_default_uses_contractual_thresholds() {
        let settings = EscalationSettings::default();

        assert_eq!(settings.consecutive_edit_failures, 3);
        assert_eq!(settings.same_file_rewrites, 5);
        assert_eq!(settings.tool_call_threshold, 200);
    }

    // Given: PathBuf を含むエスカレーションメモ / When: JSON 往復 / Then: 同じメモへ復元される
    #[test]
    fn escalation_memo_serde_round_trip_preserves_paths() {
        let memo = EscalationMemo {
            source_run_id: RunId::new(7),
            original_request: "調査を完了する".to_string(),
            findings: vec!["依存関係を確認した".to_string()],
            files_touched: vec![PathBuf::from("crates/runtime/src/lib.rs")],
            blockers: vec!["権限が不足している".to_string()],
            workspace_state: "M crates/runtime/src/lib.rs".to_string(),
            escalation_reason: "編集失敗が連続した".to_string(),
            suggested_next: "Orchestrator で担当を分割する".to_string(),
        };

        let json = serde_json::to_string(&memo).expect("serialize escalation memo");
        let restored: EscalationMemo =
            serde_json::from_str(&json).expect("deserialize escalation memo");

        assert_eq!(restored, memo);
    }
}
