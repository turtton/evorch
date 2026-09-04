//! web_fetch の 3 層 AND network 判定における単層 deny 経路のテスト (AC6)。
//!
//! role capability / per-tool policy / session NetworkAccess の各層が、他層が
//! すべて許可している場合でも単独で web_fetch を拒否することを検証する
//! ([`runtime::judge_web_network_access`])。

use agents::{NetworkAccess, Role, RoleCapabilities};
use runtime::{ExecutionPolicy, NetworkAccessDecision, RuntimeError, judge_web_network_access};
use sandbox::PolicyDecision;

// Given: web_fetch が role の allowed_tools に含まれない（role network Allowed・per-tool Allow・session Allowed） / When: 3層AND判定 / Then: role 層だけで Deny になる
#[test]
fn role_tool_allowlist_deny_blocks_web_fetch_alone() {
    let role = RoleCapabilities::new(["read", "grep"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_fetch",
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
fn per_tool_deny_blocks_web_fetch_alone() {
    let role = RoleCapabilities::new(["web_fetch"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_fetch",
        PolicyDecision::Deny,
        NetworkAccess::Allowed,
    );

    assert!(
        matches!(decision, NetworkAccessDecision::Deny { .. }),
        "実際の判定: {decision:?}"
    );
}

// Given: role と session は permissive だが per-tool が Ask / When: 3層AND判定 / Then: Allow ではなく Ask になる
#[test]
fn per_tool_ask_requires_approval_for_web_fetch() {
    let role = RoleCapabilities::new(["web_fetch"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_fetch",
        PolicyDecision::Ask,
        NetworkAccess::Allowed,
    );

    assert!(
        matches!(decision, NetworkAccessDecision::Ask { .. }),
        "実際の判定: {decision:?}"
    );
}

// Given: role は完全 permissive・per-tool Allow だが session が Denied / When: 3層AND判定 / Then: session 層だけで Deny になる
#[test]
fn session_deny_blocks_web_fetch_alone() {
    let role = RoleCapabilities::new(["web_fetch"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_fetch",
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
fn all_layers_permissive_allow_web_fetch() {
    let role = RoleCapabilities::new(["web_fetch"], NetworkAccess::Allowed, false);

    let decision = judge_web_network_access(
        &role,
        "TestRole",
        "web_fetch",
        PolicyDecision::AutoAllow,
        NetworkAccess::Allowed,
    );

    assert_eq!(decision, NetworkAccessDecision::Allow);
}

// Given: production の layer-1 execute gate (ExecutionPolicy::for_role) と全 5 role / When: web_fetch を authorize / Then: Librarian と Orchestrator は許可され、他の 3 role は CapabilityDenied になる (AC6)
// agents::Role に全 variant の定数はないため列挙する。現行の全 variant は
// Orchestrator / Explorer / Worker / Reviewer / Librarian の 5 つであり、
// 新 variant を enum に追加した際はこの列挙への追加が必要である
// (match と異なり追加漏れはコンパイラに検出されない)。
// web_fetch は ADR 0002 (2026-09-03 補足) により Librarian (network Allowed) と
// Orchestrator (network OptIn) に公開され、このテストは production gate の
// 公開範囲を固定する。
// role の公開範囲を変更する slice は必ず本テストを更新すること (tripwire)。
#[test]
fn production_layer1_gate_exposes_web_fetch_to_librarian_and_orchestrator() {
    for role in [Role::Librarian, Role::Orchestrator] {
        let policy = ExecutionPolicy::for_role(role);
        assert_eq!(
            policy.authorize("web_fetch"),
            Ok(()),
            "role {} の web_fetch は layer-1 execute gate で許可されるべき (AC6)",
            role.name()
        );
    }

    for role in [Role::Explorer, Role::Worker, Role::Reviewer] {
        let policy = ExecutionPolicy::for_role(role);
        let role_name = role.name();
        let Err(RuntimeError::CapabilityDenied { role, tool, reason }) =
            policy.authorize("web_fetch")
        else {
            panic!("role {role_name} の web_fetch は layer-1 execute gate で拒否されるべき (AC6)");
        };

        assert_eq!(role, role_name);
        assert_eq!(tool, "web_fetch");
        assert!(!reason.is_empty(), "拒否理由が空であってはならない");
    }
}

// Given: Librarian と Orchestrator の production ポリシー / When: ケイパビリティの network を参照する / Then: Librarian は Allowed、Orchestrator は OptIn になる (ADR 0002 2026-09-03 補足)
#[test]
fn librarian_network_is_allowed_and_orchestrator_is_opt_in() {
    assert_eq!(
        ExecutionPolicy::for_role(Role::Librarian)
            .capabilities
            .network,
        NetworkAccess::Allowed
    );
    assert_eq!(
        ExecutionPolicy::for_role(Role::Orchestrator)
            .capabilities
            .network,
        NetworkAccess::OptIn
    );
}

// Given: Orchestrator の production ポリシー (role network OptIn) / When: session の NetworkAccess を変えて web_fetch を 3層AND判定 / Then: session OptIn では承認理由に session を含む Ask、Denied では Deny、Allowed かつ per-tool AutoAllow では Allow になる (AC6)
#[test]
fn orchestrator_web_fetch_requires_session_opt_in_approval() {
    let policy = ExecutionPolicy::for_role(Role::Orchestrator);

    let decision = judge_web_network_access(
        &policy.capabilities,
        &policy.role_name,
        "web_fetch",
        PolicyDecision::Ask,
        NetworkAccess::OptIn,
    );
    let NetworkAccessDecision::Ask { reason } = decision else {
        panic!("session OptIn では Ask になるべき。実際の判定: {decision:?}");
    };
    assert!(
        reason.contains("session"),
        "Ask 理由は session を含むべき: {reason}"
    );

    let decision = judge_web_network_access(
        &policy.capabilities,
        &policy.role_name,
        "web_fetch",
        PolicyDecision::Ask,
        NetworkAccess::Denied,
    );
    assert!(
        matches!(decision, NetworkAccessDecision::Deny { .. }),
        "session Denied では Deny になるべき。実際の判定: {decision:?}"
    );

    let decision = judge_web_network_access(
        &policy.capabilities,
        &policy.role_name,
        "web_fetch",
        PolicyDecision::AutoAllow,
        NetworkAccess::Allowed,
    );
    assert_eq!(
        decision,
        NetworkAccessDecision::Allow,
        "role の OptIn は通過扱いのため session Allowed では Allow になるべき"
    );
}
