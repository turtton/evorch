//! v0.2 エージェントロールの定義 (ADR 0002)。
//!
//! 各ロールは personality ではなく capability boundary として定義される。

use crate::capability::RoleCapabilities;
use serde::{Deserialize, Serialize};

/// v0.2 のエージェントロール (ADR 0002 の capability boundary)。
///
/// # 境界の一覧
///
/// | Role | ツール | 委譲 |
/// |---|---|---|
/// | Orchestrator | 委譲・調査・skill_load・AgentRun 間メッセージ系 + web_fetch (mutation tool なし) | 可 |
/// | Explorer | read / grep | 不可 |
/// | Worker | read / write / edit / grep / shell / git_diff / skill_load + AgentRun 間メッセージ系 | 不可 |
/// | Reviewer | read / grep / git_diff | 不可 |
/// | WebResearcher | read / grep / web_search / web_fetch | 不可 |
///
/// # 設計上の決定
///
/// - Reviewer のツールセットはワークスペースの決定であり、intents では未定義。
/// - web_search は WebResearcher 専用であり、Orchestrator は web_fetch のみを持つ。
///
/// # ロール追加時の配線
///
/// 1. この enum と名前の変換に variant を追加する。
/// 2. [`Role::capabilities`] に対応する arm を 1 つ追加する
///    (網羅的 match によりコンパイラが追加を強制する)。
/// 3. delegate の解析・schema、設定バインディング、基準プロンプト・catalog、
///    GUI のロール選択へ登録する。
///
/// ツール権限の強制は [`RoleCapabilities`] を消費する共通の仕組みであり、
/// ロールごとの強制ロジックを追加する必要はない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// 調整役。委譲のみを行い、mutation tool を持たない。
    Orchestrator,
    /// 調査役。読み取り専用。
    Explorer,
    /// 実装役。ワークスペースの read-write を持つ。
    Worker,
    /// レビュー役。生成と独立したレビューを行う。
    Reviewer,
    /// 外部情報の調査役。read / grep / web_search / web_fetch を持つ。
    WebResearcher,
    Planner,
    Oracle,
    MultimodalLooker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRole {
    pub name: String,
}

impl std::fmt::Display for UnknownRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown role: {}", self.name)
    }
}

impl std::error::Error for UnknownRole {}

impl Role {
    pub fn from_name(name: &str) -> Result<Self, UnknownRole> {
        match name {
            "Orchestrator" => Ok(Self::Orchestrator),
            "Explorer" => Ok(Self::Explorer),
            "Worker" => Ok(Self::Worker),
            "Reviewer" => Ok(Self::Reviewer),
            "WebResearcher" => Ok(Self::WebResearcher),
            "Planner" => Ok(Self::Planner),
            "Oracle" => Ok(Self::Oracle),
            "MultimodalLooker" => Ok(Self::MultimodalLooker),
            _ => Err(UnknownRole {
                name: name.to_owned(),
            }),
        }
    }

    /// ロール名識別子。
    pub const fn name(&self) -> &'static str {
        match self {
            Role::Orchestrator => "Orchestrator",
            Role::Explorer => "Explorer",
            Role::Worker => "Worker",
            Role::Reviewer => "Reviewer",
            Role::WebResearcher => "WebResearcher",
            Role::Planner => "Planner",
            Role::Oracle => "Oracle",
            Role::MultimodalLooker => "MultimodalLooker",
        }
    }

    /// ADR 0002 のケイパビリティ行列を返す。
    pub fn capabilities(&self) -> RoleCapabilities {
        let mut capabilities = match self {
            Role::Orchestrator => RoleCapabilities::new(
                [
                    "delegate",
                    "send_message",
                    "skill_load",
                    "send",
                    "wait_reply",
                    "inbox",
                    "wait",
                    "run_output",
                    "cancel",
                    "list_agents",
                    "inspect_agent",
                    "subagent_questions",
                    "answer_subagent_question",
                    "read",
                    "grep",
                    "git_diff",
                    "compact",
                    "finish",
                    "web_fetch",
                ],
                true,
            ),
            Role::Explorer => RoleCapabilities::new(["read", "grep"], false),
            Role::Worker => RoleCapabilities::new(
                [
                    "read",
                    "edit",
                    "write",
                    "grep",
                    "shell",
                    "skill_load",
                    "git_diff",
                    "send",
                    "wait_reply",
                    "inbox",
                    "escalate",
                ],
                false,
            ),
            Role::Reviewer => {
                RoleCapabilities::new(["read", "grep", "git_diff", "submit_review"], false)
            }
            Role::WebResearcher => {
                RoleCapabilities::new(["read", "grep", "web_search", "web_fetch"], false)
            }
            Role::Planner => RoleCapabilities::new(
                ["read", "grep", "git_diff", "skill_load", "web_fetch"],
                false,
            ),
            Role::Oracle => RoleCapabilities::new(["read", "grep", "git_diff"], false),
            Role::MultimodalLooker => RoleCapabilities::new(["read"], false),
        };
        capabilities
            .allowed_tools
            .extend(["ledger_append", "ledger_read", "ask_user", "user_answers"].map(String::from));
        capabilities
    }
}
