//! ネットワーク境界のサンドボックス実行モードへの純粋マッピングとサンドボックス構築
//! (ADR 0002 / 0021)。
//!
//! このモジュールは [`NetworkAccess`] 要件を [`SandboxNetworkMode`] へ解決する
//! 純粋な写像と、その解決結果を [`build_sandbox`] で bwrap 構成へ伝達するシームを
//! 提供する。サンドボックス化コマンドの実行方法 (executor) は扱わない。

use std::{path::PathBuf, sync::Arc};

use agents::NetworkAccess;
use sandbox::{BwrapConfig, BwrapSandbox, Sandbox, SandboxError};

use crate::policy::ExecutionPolicy;

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
