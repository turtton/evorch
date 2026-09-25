//! Role exposure and tool-call authorization for web_search.

use agents::{Role, RoleCapabilities};
use runtime::{ExecutionPolicy, NetworkAccessDecision, RuntimeError, judge_web_network_access};
use sandbox::PolicyDecision;

#[test]
fn unavailable_tool_is_denied_even_with_permissive_policy() {
    let role = RoleCapabilities::new(["read", "grep"], false);
    assert!(matches!(
        judge_web_network_access(
            &role,
            "TestRole",
            "web_search",
            PolicyDecision::AutoAllow,
            true
        ),
        NetworkAccessDecision::Deny { .. },
    ));
}

#[test]
fn disabled_web_tools_and_per_tool_deny_each_block_execution() {
    let role = RoleCapabilities::new(["web_search"], false);
    for (enabled, policy) in [
        (false, PolicyDecision::AutoAllow),
        (true, PolicyDecision::Deny),
    ] {
        assert!(matches!(
            judge_web_network_access(&role, "TestRole", "web_search", policy, enabled),
            NetworkAccessDecision::Deny { .. },
        ));
    }
}

#[test]
fn search_is_automatic_unless_tool_policy_requires_review() {
    let role = RoleCapabilities::new(["web_search"], false);
    assert_eq!(
        judge_web_network_access(
            &role,
            "TestRole",
            "web_search",
            PolicyDecision::AutoAllow,
            true
        ),
        NetworkAccessDecision::Allow,
    );
    assert!(matches!(
        judge_web_network_access(&role, "TestRole", "web_search", PolicyDecision::Ask, true),
        NetworkAccessDecision::Ask { .. },
    ));
}

#[test]
fn production_roles_expose_only_their_supported_web_tools() {
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
        Role::WebResearcher,
        Role::Planner,
        Role::Oracle,
        Role::MultimodalLooker,
    ] {
        let policy = ExecutionPolicy::for_role(role);
        if [Role::WebResearcher].contains(&role) {
            assert_eq!(policy.authorize("web_search"), Ok(()));
        } else {
            assert!(
                matches!(
                    policy.authorize("web_search"),
                    Err(RuntimeError::CapabilityDenied { .. }),
                ),
                "{role:?} must not expose web_search"
            );
        }
    }
}
