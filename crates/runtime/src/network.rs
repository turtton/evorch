//! ネットワーク境界のサンドボックス実行モードへの純粋マッピングとサンドボックス構築
//! (ADR 0002 / 0021)。
//!
//! このモジュールは [`NetworkAccess`] 要件を [`SandboxNetworkMode`] へ解決する
//! 純粋な写像と、その解決結果を [`build_sandbox`] で bwrap 構成へ伝達するシームを
//! 提供する。サンドボックス化コマンドの実行方法 (executor) は扱わない。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agents::NetworkAccess;
use sandbox::{BwrapConfig, BwrapSandbox, Sandbox, SandboxError};

use crate::policy::ExecutionPolicy;
use crate::runtime::{IsolatedMounts, SandboxFactory};
use crate::workspace::OwnedWorktree;

/// サンドボックスのネットワーク実行モード (issue #19 / ADR 0021)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxNetworkMode {
    /// 新規ネットワーク名前空間 (`--unshare-net`)。v0.1 の deny 相当。
    Unshared,
    /// 親ネットワーク名前空間で実行する。完全開放である。
    ///
    /// bwrap は宛先フィルタを持たず、v0.1 はバイナリポリシーのため
    /// 許可した場合の通信先制限は存在しない (ADR 0021 参照)。
    ParentNetns,
}

/// [`NetworkAccess`] 要件から [`SandboxNetworkMode`] への純粋マッピング。
///
/// - [`NetworkAccess::Allowed`] はオプトインの有無に依らず [`SandboxNetworkMode::ParentNetns`]
/// - [`NetworkAccess::OptIn`] は明示的オプトインがある場合のみ
///   [`SandboxNetworkMode::ParentNetns`]、それ以外は [`SandboxNetworkMode::Unshared`]
/// - [`NetworkAccess::Denied`] はオプトインの有無に依らず [`SandboxNetworkMode::Unshared`]
pub fn sandbox_network_mode(access: NetworkAccess, explicit_opt_in: bool) -> SandboxNetworkMode {
    match access {
        NetworkAccess::Allowed => SandboxNetworkMode::ParentNetns,
        NetworkAccess::OptIn if explicit_opt_in => SandboxNetworkMode::ParentNetns,
        NetworkAccess::OptIn => SandboxNetworkMode::Unshared,
        NetworkAccess::Denied => SandboxNetworkMode::Unshared,
    }
}

impl ExecutionPolicy {
    /// このポリシーのネットワーク要件をサンドボックスモードへ解決する。
    ///
    /// v0.1 のポリシーにはオプトイン経路が存在しないため `explicit_opt_in` は
    /// 常に `false` で委譲する。したがって [`NetworkAccess::OptIn`] のロール
    /// (Explorer) は [`SandboxNetworkMode::Unshared`] に解決される (fail-closed)。
    pub fn sandbox_network_mode(&self) -> SandboxNetworkMode {
        sandbox_network_mode(self.capabilities.network, false)
    }
}

/// [`ExecutionPolicy`] のネットワーク境界を強制する bwrap サンドボックスを構築する。
///
/// [`ExecutionPolicy::sandbox_network_mode`] の解決結果を
/// [`BwrapConfig::allow_network`] へ伝達する。[`SandboxNetworkMode::Unshared`]
/// は `--unshare-net` 付き、[`SandboxNetworkMode::ParentNetns`] はネットワーク
/// 分離なしの構成になる。検証や構築のエラーはそのまま伝播する (fail-closed)。
/// サンドボックスなしでの実行へのフォールバックは存在しない (ADR 0021)。
///
/// これは構成時点 (composition-time) のシームである。1 つの ToolExecutor /
/// AgentRuntime インスタンスは 1 つのポリシーから構築された 1 つのサンドボックスを
/// 受け取る。実行ごと・ロールごとのサンドボックス切替には executor API の
/// 再設計が必要であり、それは v0.1 のスコープ外である (issue #19)。
pub fn build_sandbox(
    policy: &ExecutionPolicy,
    workspace_root: PathBuf,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    let config = BwrapConfig::new(workspace_root).allow_network(matches!(
        policy.sandbox_network_mode(),
        SandboxNetworkMode::ParentNetns
    ));
    BwrapSandbox::detect(config).map(|detected| Arc::new(detected) as Arc<dyn Sandbox>)
}

/// isolated worktree が git 操作に必要とする最小 mount set を構築する。
///
/// `packed-refs` の rewrite (`git pack-refs` / auto gc) は意図的に writable にしない。
/// 通常の branch 更新に必要な個別 metadata だけを writable にする最小性との trade-off。
pub(crate) fn isolated_mounts(worktree: &OwnedWorktree, git_common_dir: &Path) -> IsolatedMounts {
    let worktree_name = match worktree.path.file_name() {
        Some(name) => name,
        None => worktree.path.as_os_str(),
    };
    IsolatedMounts {
        workspace_root: worktree.path.clone(),
        ro_binds: vec![git_common_dir.to_path_buf()],
        rw_binds: vec![
            git_common_dir.join("worktrees").join(worktree_name),
            git_common_dir.join("objects"),
            git_common_dir.join("refs/heads"),
            git_common_dir.join("logs"),
        ],
    }
}

pub(crate) struct BwrapFactory;

impl SandboxFactory for BwrapFactory {
    fn build(
        &self,
        policy: &ExecutionPolicy,
        mounts: &IsolatedMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let mut config = BwrapConfig::new(mounts.workspace_root.clone()).allow_network(matches!(
            policy.sandbox_network_mode(),
            SandboxNetworkMode::ParentNetns
        ));
        for path in &mounts.ro_binds {
            config = config.ro_bind(path.clone());
        }
        for path in &mounts.rw_binds {
            config = config.rw_bind(path.clone());
        }
        BwrapSandbox::detect(config).map(|detected| Arc::new(detected) as Arc<dyn Sandbox>)
    }
}

/// role・tool・session の3層AND判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkAccessDecision {
    /// 全層が自動許可した。
    Allow,
    /// deny はないが、少なくとも1層で承認が必要。
    Ask {
        /// 承認が必要な理由。
        reason: String,
    },
    /// いずれかの層が拒否した。
    Deny {
        /// 最初に拒否した層の理由。
        reason: String,
    },
}

/// web network tool に対する role・tool・session の3層AND判定を行う。
///
/// deny は role → tool → session の順で最優先し、deny がなければ ask 理由を結合する。
/// role 層で [`NetworkAccess::OptIn`] は通過扱いとし、明示的オプトインの ask 表現は
/// session 層の [`NetworkAccess::OptIn`] が担う（契約の層割り当てに従う）。
pub fn judge_web_network_access(
    role: &agents::RoleCapabilities,
    role_name: &str,
    tool: &str,
    per_tool: sandbox::PolicyDecision,
    session: NetworkAccess,
) -> NetworkAccessDecision {
    match role.check_tool(role_name, tool) {
        agents::CapabilityDecision::Allowed => {}
        agents::CapabilityDecision::Denied { reason, .. } => {
            return NetworkAccessDecision::Deny { reason };
        }
    }
    match role.network {
        NetworkAccess::Denied => {
            return NetworkAccessDecision::Deny {
                reason: format!(
                    "role '{role_name}' のネットワーク利用は禁止されています (ADR 0002)"
                ),
            };
        }
        NetworkAccess::OptIn | NetworkAccess::Allowed => {}
    }

    let mut ask_reasons = Vec::new();
    match per_tool {
        sandbox::PolicyDecision::AutoAllow => {}
        sandbox::PolicyDecision::Ask => {
            ask_reasons.push(format!("ツール '{tool}' の実行には承認が必要です"));
        }
        sandbox::PolicyDecision::Deny => {
            return NetworkAccessDecision::Deny {
                reason: format!("ツール '{tool}' のネットワーク権限は拒否されています"),
            };
        }
    }
    match session {
        NetworkAccess::Allowed => {}
        NetworkAccess::OptIn => {
            ask_reasons.push("session のネットワーク利用には承認が必要です".to_owned());
        }
        NetworkAccess::Denied => {
            return NetworkAccessDecision::Deny {
                reason: "session のネットワーク利用は禁止されています".to_owned(),
            };
        }
    }

    if ask_reasons.is_empty() {
        NetworkAccessDecision::Allow
    } else {
        NetworkAccessDecision::Ask {
            reason: ask_reasons.join(" / "),
        }
    }
}

#[cfg(test)]
mod network_access_tests {
    use super::*;
    use agents::RoleCapabilities;
    use sandbox::PolicyDecision;

    #[derive(Clone, Copy)]
    enum Expected {
        Allow,
        Ask,
        Deny,
    }

    // Given: 各層の allow・ask・deny 組合せ / When: 3層AND判定 / Then: deny優先・ask集約・全通過allowになる
    #[test]
    fn judges_all_three_layers_fail_closed() {
        let cases = [
            (
                "worker network deny",
                true,
                NetworkAccess::Denied,
                PolicyDecision::AutoAllow,
                NetworkAccess::Allowed,
                Expected::Deny,
            ),
            (
                "tool missing",
                false,
                NetworkAccess::Allowed,
                PolicyDecision::AutoAllow,
                NetworkAccess::Allowed,
                Expected::Deny,
            ),
            (
                "per-tool deny",
                true,
                NetworkAccess::Allowed,
                PolicyDecision::Deny,
                NetworkAccess::Allowed,
                Expected::Deny,
            ),
            (
                "session deny",
                true,
                NetworkAccess::Allowed,
                PolicyDecision::AutoAllow,
                NetworkAccess::Denied,
                Expected::Deny,
            ),
            (
                "session opt-in",
                true,
                NetworkAccess::Allowed,
                PolicyDecision::AutoAllow,
                NetworkAccess::OptIn,
                Expected::Ask,
            ),
            (
                "per-tool ask",
                true,
                NetworkAccess::Allowed,
                PolicyDecision::Ask,
                NetworkAccess::Allowed,
                Expected::Ask,
            ),
            (
                "ask plus deny",
                true,
                NetworkAccess::Allowed,
                PolicyDecision::Ask,
                NetworkAccess::Denied,
                Expected::Deny,
            ),
            (
                "all pass",
                true,
                NetworkAccess::Allowed,
                PolicyDecision::AutoAllow,
                NetworkAccess::Allowed,
                Expected::Allow,
            ),
        ];

        for (name, has_tool, role_network, per_tool, session, expected) in cases {
            let tools = if has_tool {
                &["web_fetch"][..]
            } else {
                &[][..]
            };
            let role = RoleCapabilities::new(tools.iter().copied(), role_network, false);
            let decision =
                judge_web_network_access(&role, "TestRole", "web_fetch", per_tool, session);
            match expected {
                Expected::Allow => assert_eq!(decision, NetworkAccessDecision::Allow, "{name}"),
                Expected::Ask => assert!(
                    matches!(decision, NetworkAccessDecision::Ask { .. }),
                    "{name}"
                ),
                Expected::Deny => assert!(
                    matches!(decision, NetworkAccessDecision::Deny { .. }),
                    "{name}"
                ),
            }
        }
    }

    // Given: per-tool ask と session OptIn / When: 3層AND判定 / Then: 両方の承認理由が結合される
    #[test]
    fn combines_ask_reasons() {
        let role = RoleCapabilities::new(["web_fetch"], NetworkAccess::Allowed, false);
        let decision = judge_web_network_access(
            &role,
            "Librarian",
            "web_fetch",
            PolicyDecision::Ask,
            NetworkAccess::OptIn,
        );
        let NetworkAccessDecision::Ask { reason } = decision else {
            panic!("ask 判定でなければならない");
        };
        assert!(reason.contains("ツール 'web_fetch'"));
        assert!(reason.contains("session"));
    }
}
