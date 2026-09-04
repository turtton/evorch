//! ランタイム層のエラー型を定義します。

use event_bus::AgentRunPhase;

use crate::RunId;

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

    /// サンドボックス構築に失敗した (fail-closed, ADR 0021)。
    #[error("サンドボックス構築に失敗しました: {detail}")]
    Sandbox { detail: String },

    /// workspace project の初期化に失敗した。
    #[error("workspace の初期化に失敗しました: {detail}")]
    Workspace { detail: String },

    /// 送信者と受信者の親子関係またはメッセージ種別のルールにより配送が拒否された。
    #[error("AgentRun {sender} から {recipient} へのメッセージが拒否されました: {detail}")]
    MessageDenied {
        /// 拒否されたメッセージの送信元 run ID。
        sender: RunId,
        /// 拒否されたメッセージの宛先 run ID。
        recipient: RunId,
        /// 拒否理由。
        detail: String,
    },

    /// 指定されたメッセージ ID に対応する相関関係が存在しない。
    #[error("未知のメッセージ ID です: {message_id}")]
    UnknownMessage {
        /// 存在しなかったメッセージ ID。
        message_id: String,
    },

    /// 返信待ちがタイムアウトした。
    #[error("返信待ちがタイムアウトしました: {message_id}")]
    ReplyTimeout {
        /// 待機していた元メッセージ ID。
        message_id: String,
    },

    /// 受信者 run の mailbox が一杯である。
    #[error("AgentRun {run_id} の mailbox が一杯です")]
    MailboxFull {
        /// mailbox が一杯だった run ID。
        run_id: String,
    },

    /// 対象 run でコンテキスト圧縮が実行中である。
    #[error("AgentRun {run_id} で compaction が実行中です")]
    CompactionInFlight {
        /// compaction 実行中の run ID。
        run_id: String,
    },
}
