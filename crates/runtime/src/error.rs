//! ランタイム層のエラー型を定義します。

use event_bus::AgentRunPhase;

/// エージェント実行ランタイムのエラー。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RuntimeError {
    /// 存在しない AgentRun が参照された。
    #[error("未知の AgentRun が指定されました: {run_id}")]
    UnknownRun {
        /// 参照された実行 ID。
        run_id: String,
    },

    /// ロールの capability boundary (ADR 0002) によりツール使用が拒否された。
    #[error("ロール '{role}' のツール '{tool}' が拒否されました: {reason}")]
    CapabilityDenied {
        /// 拒否されたロール名。
        role: String,
        /// 拒否されたツール名。
        tool: String,
        /// 拒否理由。
        reason: String,
    },

    /// AgentRun の位相状態機械として不正な遷移が要求された。
    #[error("不正な位相遷移です: {from:?} -> {to:?}")]
    InvalidTransition {
        /// 遷移元の位相。
        from: AgentRunPhase,
        /// 遷移先の位相。
        to: AgentRunPhase,
    },

    /// 既に終了した AgentRun へ操作が要求された。
    #[error("実行 {run_id} は既に終了しています")]
    RunTerminated {
        /// 終了済みの実行 ID。
        run_id: String,
    },

    /// モデル呼び出し (境界の実装側) の失敗。
    #[error("モデル呼び出しに失敗しました: {reason}")]
    Model {
        /// 失敗理由。
        reason: String,
    },

    /// ツール実行の失敗。
    #[error(transparent)]
    Tool(#[from] tools::ToolError),
}
