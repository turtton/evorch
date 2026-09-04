//! v0.2 エージェントロールの定義 (ADR 0002)。
//!
//! 各ロールは personality ではなく capability boundary として定義される。

use crate::capability::{NetworkAccess, RoleCapabilities};
use serde::{Deserialize, Serialize};

/// v0.2 のエージェントロール (ADR 0002 の capability boundary)。
///
/// # 境界の一覧
///
/// | Role | ツール | ネットワーク | 委譲 |
/// |---|---|---|---|
/// | Orchestrator | 委譲・調査・skill_load・AgentRun 間メッセージ系 + web_fetch (mutation tool なし) | OptIn | 可 |
/// | Explorer | read / grep | OptIn | 不可 |
/// | Worker | read / edit / grep / shell / git_diff / skill_load + AgentRun 間メッセージ系 | Denied | 不可 |
/// | Reviewer | read / grep / git_diff | Denied | 不可 |
/// | Librarian | read / grep / web_search / web_fetch | Allowed | 不可 |
///
/// # 設計上の決定
///
/// - Reviewer のツールセットはワークスペースの決定であり、intents では未定義。
/// - Orchestrator のネットワークは web_fetch のみを対象とする
///   [`NetworkAccess::OptIn`] (ADR 0002 2026-09-03 補足)。
///   web_search は Librarian 専用であり、Orchestrator は持たない。
///
/// # v0.2 拡張レシピ
///
/// Librarian はこのレシピに従い追加済み。将来の新ロール (Oracle など) の追加も
/// ロール定義の追加のみで完結する:
///
/// 1. この enum に variant を追加する。
/// 2. [`Role::capabilities`] に対応する arm を 1 つ追加する
///    (網羅的 match によりコンパイラが追加を強制する)。
///
/// ランタイムの境界強制は [`RoleCapabilities`] のみを消費し、`Role` に対して
/// マッチングしない。そのため既存ロールの強制ロジックには変更が不要である。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// 調整役。委譲のみを行い、mutation tool を持たない。
    Orchestrator,
    /// 調査役。読み取り専用で、ネットワークはオプトイン。
    Explorer,
    /// 実装役。ワークスペースの read-write を持つ。
    Worker,
    /// レビュー役。生成と独立したレビューを行う。
    Reviewer,
    /// 調査役 (v0.2)。read / grep と web_search / web_fetch を持ち、ネットワークは常時許可。
    Librarian,
}

impl Role {
    /// ロール名識別子。
    pub const fn name(&self) -> &'static str {
        match self {
            Role::Orchestrator => "Orchestrator",
            Role::Explorer => "Explorer",
            Role::Worker => "Worker",
            Role::Reviewer => "Reviewer",
            Role::Librarian => "Librarian",
        }
    }

    /// ADR 0002 のケイパビリティ行列を返す。
    pub fn capabilities(&self) -> RoleCapabilities {
        match self {
            Role::Orchestrator => RoleCapabilities::new(
                [
                    "delegate",
                    "delegate_background",
                    "send_message",
                    "skill_load",
                    "send",
                    "wait_reply",
                    "inbox",
                    "wait",
                    "cancel",
                    "list_agents",
                    "inspect_agent",
                    "read",
                    "grep",
                    "git_diff",
                    "compact",
                    "finish",
                    "web_fetch",
                ],
                NetworkAccess::OptIn,
                true,
            ),
            Role::Explorer => RoleCapabilities::new(["read", "grep"], NetworkAccess::OptIn, false),
            Role::Worker => RoleCapabilities::new(
                [
                    "read",
                    "edit",
                    "grep",
                    "shell",
                    "skill_load",
                    "git_diff",
                    "send",
                    "wait_reply",
                    "inbox",
                ],
                NetworkAccess::Denied,
                false,
            ),
            Role::Reviewer => {
                RoleCapabilities::new(["read", "grep", "git_diff"], NetworkAccess::Denied, false)
            }
            Role::Librarian => RoleCapabilities::new(
                ["read", "grep", "web_search", "web_fetch"],
                NetworkAccess::Allowed,
                false,
            ),
        }
    }
}
