//! web_search の 3 層 AND network 判定における単層 deny 経路のテスト (AC6)。
//!
//! role capability / per-tool policy / session NetworkAccess の各層が、他層が
//! すべて許可している場合でも単独で web_search を拒否することを検証する
//! ([`runtime::judge_web_network_access`])。

use agents::{NetworkAccess, Role, RoleCapabilities};
use runtime::{ExecutionPolicy, NetworkAccessDecision, RuntimeError, judge_web_network_access};
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

// Given: production の layer-1 execute gate (ExecutionPolicy::for_role) と全 5 role / When: web_search を authorize / Then: Librarian のみ許可され、他の 4 role は CapabilityDenied になる (AC6)
// agents::Role に全 variant の定数はないため列挙する。現行の全 variant は
// Orchestrator / Explorer / Worker / Reviewer / Librarian の 5 つであり、
// 新 variant を enum に追加した際はこの列挙への追加が必要である
// (match と異なり追加漏れはコンパイラに検出されない)。
// web_search は ADR 0002 (2026-09-03 補足) により Librarian 専用であり、
// このテストは production gate が Librarian にのみ web_search を公開し、
// 他の全 role で拒否することを固定する。
// role の公開範囲を変更する slice は必ず本テストを更新すること (tripwire)。
#[test]
fn production_layer1_gate_exposes_web_search_only_to_librarian() {
    let policy = ExecutionPolicy::for_role(Role::Librarian);
    assert_eq!(
        policy.authorize("web_search"),
        Ok(()),
        "Librarian の web_search は layer-1 execute gate で許可されるべき (AC6)"
    );

    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
    ] {
        let policy = ExecutionPolicy::for_role(role);
        let role_name = role.name();
        let Err(RuntimeError::CapabilityDenied { role, tool, reason }) =
            policy.authorize("web_search")
        else {
            panic!("role {role_name} の web_search は layer-1 execute gate で拒否されるべき (AC6)");
        };

        assert_eq!(role, role_name);
        assert_eq!(tool, "web_search");
        assert!(!reason.is_empty(), "拒否理由が空であってはならない");
    }
}

// Given: Orchestrator の production ポリシー (web_search は allowed_tools 外) / When: session NetworkAccess が Allowed / OptIn / Denied のいずれでも 3層AND判定 / Then: すべて Deny になる
#[test]
fn orchestrator_web_search_denied_regardless_of_session() {
    let policy = ExecutionPolicy::for_role(Role::Orchestrator);

    for session in [
        NetworkAccess::Allowed,
        NetworkAccess::OptIn,
        NetworkAccess::Denied,
    ] {
        let decision = judge_web_network_access(
            &policy.capabilities,
            &policy.role_name,
            "web_search",
            PolicyDecision::AutoAllow,
            session,
        );

        assert!(
            matches!(decision, NetworkAccessDecision::Deny { .. }),
            "session {session:?} でも Deny になるべき。実際の判定: {decision:?}"
        );
    }
}
