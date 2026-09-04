//! ケイパビリティ境界のデータ定義と判定チェッカー (ADR 0002)。
//!
//! このモジュールはロールに依存しない純粋なデータ構造のみを扱い、I/O も async も持たない。
//! ランタイムの境界強制は [`RoleCapabilities`] のみを消費する。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// ロールのネットワークアクセス要件 (ADR 0002)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NetworkAccess {
    /// ネットワークアクセスを禁止する (ADR 0008 default-deny)。既定値 (ADR 0008 default-deny)。
    #[default]
    Denied,
    /// 明示的なオプトイン時のみ許可する。
    OptIn,
    /// 常に許可する (Librarian 等)。
    Allowed,
}

/// ロールのケイパビリティ集合。ADR 0002 の capability boundary をデータ化したもの。
///
/// ロール名は持たず、ツール集合・ネットワーク要件・委譲可否のみを持つ。
/// この構造はロール定義 ([`crate::role::Role`]) から独立しているため、
/// v0.2 の新ロールもこの定義を追加するだけで境界チェックに乗る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCapabilities {
    /// 許可されるツール名の集合。
    pub allowed_tools: BTreeSet<String>,
    /// ネットワークアクセス要件。
    pub network: NetworkAccess,
    /// 他エージェントへの委譲可否。
    pub can_delegate: bool,
}

impl RoleCapabilities {
    /// ツール名のイテレータからケイパビリティを構築するヘルパーコンストラクタ。
    pub fn new(
        tools: impl IntoIterator<Item = impl Into<String>>,
        network: NetworkAccess,
        can_delegate: bool,
    ) -> Self {
        Self {
            allowed_tools: tools.into_iter().map(Into::into).collect(),
            network,
            can_delegate,
        }
    }

    /// ツールの使用がこのケイパビリティ境界内か判定する。
    ///
    /// 許可されたツールは [`CapabilityDecision::Allowed`] を、それ以外は
    /// ロール名・ツール名・理由を格納した [`CapabilityDecision::Denied`] を返す。
    pub fn check_tool(&self, role_name: &str, tool: &str) -> CapabilityDecision {
        if self.allowed_tools.contains(tool) {
            CapabilityDecision::Allowed
        } else {
            CapabilityDecision::Denied {
                role_name: role_name.to_owned(),
                tool: tool.to_owned(),
                reason: format!(
                    "role '{role_name}' はツール '{tool}' を許可されていない (ADR 0002)"
                ),
            }
        }
    }
}

/// ツール使用可否の判定結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityDecision {
    /// 境界内のため許可する。
    Allowed,
    /// 境界外のため拒否する。
    Denied {
        /// 判定対象のロール名。
        role_name: String,
        /// 要求されたツール名。
        tool: String,
        /// 拒否理由 (日本語)。
        reason: String,
    },
}
