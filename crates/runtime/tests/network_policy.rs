//! ネットワーク境界からサンドボックス実行モードへのマッピング (issue #19 / ADR 0021)。
//!
//! [`NetworkAccess`] (ADR 0002 の capability boundary) を `SandboxNetworkMode`
//! (bwrap の netns 実行形態) へ解決する純粋変換と [`ExecutionPolicy`] 経由の
//! 解決を検証する。bwrap の起動は対象外 (後続タスクで扱う)。

use agents::{NetworkAccess, Role, RoleCapabilities};
use runtime::{ExecutionPolicy, SandboxNetworkMode, sandbox_network_mode};

// Given: NetworkAccess::Allowed (ロール定義レベルで常に許可)
// When: explicit_opt_in = false でマッピングする
// Then: ParentNetns に解決される
#[test]
fn allowed_without_explicit_opt_in_maps_to_parent_netns() {
    let mode = sandbox_network_mode(NetworkAccess::Allowed, false);

    assert_eq!(mode, SandboxNetworkMode::ParentNetns);
}

// Given: NetworkAccess::Allowed (ロール定義レベルで常に許可)
// When: explicit_opt_in = true でマッピングする
// Then: ParentNetns に解決される
#[test]
fn allowed_with_explicit_opt_in_maps_to_parent_netns() {
    let mode = sandbox_network_mode(NetworkAccess::Allowed, true);

    assert_eq!(mode, SandboxNetworkMode::ParentNetns);
}

// Given: NetworkAccess::OptIn (明示的オプトイン時のみ許可)
// When: explicit_opt_in = false でマッピングする
// Then: Unshared に解決される
#[test]
fn opt_in_without_explicit_opt_in_maps_to_unshared() {
    let mode = sandbox_network_mode(NetworkAccess::OptIn, false);

    assert_eq!(mode, SandboxNetworkMode::Unshared);
}

// Given: NetworkAccess::OptIn (明示的オプトイン時のみ許可)
// When: explicit_opt_in = true でマッピングする
// Then: ParentNetns に解決される
#[test]
fn opt_in_with_explicit_opt_in_maps_to_parent_netns() {
    let mode = sandbox_network_mode(NetworkAccess::OptIn, true);

    assert_eq!(mode, SandboxNetworkMode::ParentNetns);
}

// Given: NetworkAccess::Denied (ADR 0008 default-deny)
// When: explicit_opt_in = false でマッピングする
// Then: Unshared に解決される
#[test]
fn denied_without_explicit_opt_in_maps_to_unshared() {
    let mode = sandbox_network_mode(NetworkAccess::Denied, false);

    assert_eq!(mode, SandboxNetworkMode::Unshared);
}

// Given: NetworkAccess::Denied (ADR 0008 default-deny)
// When: explicit_opt_in = true でマッピングする
// Then: オプトインがあっても Unshared のまま解決される
#[test]
fn denied_with_explicit_opt_in_maps_to_unshared() {
    let mode = sandbox_network_mode(NetworkAccess::Denied, true);

    assert_eq!(mode, SandboxNetworkMode::Unshared);
}

// Given: ネットワーク要件が Denied の 3 ロール (Worker / Orchestrator / Reviewer) のポリシー
// When: sandbox_network_mode を呼ぶ
// Then: すべて Unshared に解決される
#[test]
fn for_role_denied_roles_map_to_unshared() {
    for role in [Role::Worker, Role::Orchestrator, Role::Reviewer] {
        let policy = ExecutionPolicy::for_role(role);

        assert_eq!(
            policy.sandbox_network_mode(),
            SandboxNetworkMode::Unshared,
            "{} は Unshared に解決されるべき",
            role.name()
        );
    }
}

// Given: Explorer のポリシー (NetworkAccess::OptIn)
// When: sandbox_network_mode を呼ぶ (v0.1 にはオプトイン経路がない)
// Then: fail-closed により Unshared に解決される
#[test]
fn for_role_explorer_maps_to_unshared_fail_closed() {
    let policy = ExecutionPolicy::for_role(Role::Explorer);

    assert_eq!(policy.sandbox_network_mode(), SandboxNetworkMode::Unshared);
}

// Given: role_name は Worker だがケイパビリティの network が Allowed の手組みポリシー
// When: sandbox_network_mode を呼ぶ
// Then: ロール名ではなくケイパビリティ境界に従い ParentNetns に解決される
#[test]
fn hand_built_policy_with_allowed_network_maps_to_parent_netns() {
    let policy = ExecutionPolicy {
        capabilities: RoleCapabilities::new(["read"], NetworkAccess::Allowed, false),
        role_name: "Worker".to_string(),
    };

    assert_eq!(
        policy.sandbox_network_mode(),
        SandboxNetworkMode::ParentNetns
    );
}
