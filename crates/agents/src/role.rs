//! v0.1 エージェントロールの定義 (ADR 0002)。
//!
//! 各ロールは personality ではなく capability boundary として定義される。

use crate::capability::{NetworkAccess, RoleCapabilities};
use serde::{Deserialize, Serialize};

/// v0.1 のエージェントロール (ADR 0002 の capability boundary)。
///
/// # 境界の一覧
///
/// | Role | ツール | ネットワーク | 委譲 |
/// |---|---|---|---|
/// | Orchestrator | 委譲・調査系のみ (mutation tool なし) | Denied | 可 |
/// | Explorer | read / grep | OptIn | 不可 |
/// | Worker | read / edit / grep / shell / git_diff | Denied | 不可 |
/// | Reviewer | read / grep / git_diff | Denied | 不可 |
///
/// # 設計上の決定
///
/// - Reviewer のツールセットはワークスペースの決定であり、intents では未定義。
/// - Orchestrator のネットワークが [`NetworkAccess::Denied`] なのは
///   ADR 0008 の default-deny に基づく。
///
/// # v0.2 拡張レシピ
///
/// 新ロール (Librarian / Oracle) の追加はロール定義の追加のみで完結する:
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
}

impl Role {
    /// ロール名識別子。
    pub const fn name(&self) -> &'static str {
        match self {
            Role::Orchestrator => "Orchestrator",
            Role::Explorer => "Explorer",
            Role::Worker => "Worker",
            Role::Reviewer => "Reviewer",
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
                    "wait",
                    "cancel",
                    "list_agents",
                    "inspect_agent",
                    "read",
                    "grep",
                    "git_diff",
                    "compact",
                    "finish",
                ],
                NetworkAccess::Denied,
                true,
            ),
            Role::Explorer => RoleCapabilities::new(["read", "grep"], NetworkAccess::OptIn, false),
            Role::Worker => RoleCapabilities::new(
                ["read", "edit", "grep", "shell", "git_diff"],
                NetworkAccess::Denied,
                false,
            ),
            Role::Reviewer => {
                RoleCapabilities::new(["read", "grep", "git_diff"], NetworkAccess::Denied, false)
            }
        }
    }
}
