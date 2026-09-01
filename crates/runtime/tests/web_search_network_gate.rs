//! web_search の 3 層 AND network 判定における単層 deny 経路のテスト (AC6)。
//!
//! role capability / per-tool policy / session NetworkAccess の各層が、他層が
//! すべて許可している場合でも単独で web_search を拒否することを検証する
//! ([`runtime::judge_web_network_access`])。

use agents::{NetworkAccess, RoleCapabilities};
use runtime::{NetworkAccessDecision, judge_web_network_access};
use sandbox::PolicyDecision;

// Given: web_search が role の allowed_tools に含まれない（role network Allowed・per-tool Allow・session Allowed） / When: 3層AND判定 / Then: role 層だけで Deny になる
#[test]
fn role_tool_allowlist_deny_blocks_web_search_alone() {
    let role = RoleCapabilities::new(["read", "grep"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_search",
        PolicyDecision::AutoAllow,
        NetworkAccess::Allowed,
    );

    assert!(
        matches!(decision, NetworkAccessDecision::Deny { .. }),
        "実際の判定: {decision:?}"
    );
}

// Given: role の network が Denied（web_search は allowed・per-tool Allow・session Allowed） / When: 3層AND判定 / Then: role network 層だけで Deny になる
#[test]
fn role_network_deny_blocks_web_search_alone() {
    let role = RoleCapabilities::new(["web_search"], NetworkAccess::Denied, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_search",
        PolicyDecision::AutoAllow,
        NetworkAccess::Allowed,
    );

    assert!(
        matches!(decision, NetworkAccessDecision::Deny { .. }),
        "実際の判定: {decision:?}"
    );
}

// Given: role は完全 permissive・session Allowed だが per-tool が Deny / When: 3層AND判定 / Then: per-tool 層だけで Deny になる
#[test]
fn per_tool_deny_blocks_web_search_alone() {
    let role = RoleCapabilities::new(["web_search"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_search",
        PolicyDecision::Deny,
        NetworkAccess::Allowed,
    );

    assert!(
        matches!(decision, NetworkAccessDecision::Deny { .. }),
        "実際の判定: {decision:?}"
    );
}

// Given: role は完全 permissive・per-tool Allow だが session が Denied / When: 3層AND判定 / Then: session 層だけで Deny になる
#[test]
fn session_deny_blocks_web_search_alone() {
    let role = RoleCapabilities::new(["web_search"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_search",
        PolicyDecision::AutoAllow,
        NetworkAccess::Denied,
    );

    assert!(
        matches!(decision, NetworkAccessDecision::Deny { .. }),
        "実際の判定: {decision:?}"
    );
}

// Given: 3 層すべて permissive / When: 3層AND判定 / Then: Allow になる（陽性対照）
#[test]
fn all_layers_permissive_allow_web_search() {
    let role = RoleCapabilities::new(["web_search"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_search",
        PolicyDecision::AutoAllow,
        NetworkAccess::Allowed,
    );

    assert_eq!(decision, NetworkAccessDecision::Allow);
}
