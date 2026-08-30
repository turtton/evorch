//! ネットワーク境界のサンドボックス実行モードへの純粋マッピングとサンドボックス構築
//! (ADR 0002 / 0021)。
//!
//! このモジュールは [`NetworkAccess`] 要件を [`SandboxNetworkMode`] へ解決する
//! 純粋な写像を提供する。サンドボックス化コマンドの実行方法 (executor) は扱わない。

use agents::NetworkAccess;

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
