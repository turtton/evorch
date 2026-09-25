//! Role exposure and tool-call authorization for web_fetch.

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
            "web_fetch",
            PolicyDecision::AutoAllow,
            true
        ),
        NetworkAccessDecision::Deny { .. },
    ));
}

#[test]
fn disabled_web_tools_and_per_tool_deny_each_block_execution() {
    let role = RoleCapabilities::new(["web_fetch"], false);
    for (enabled, policy) in [
        (false, PolicyDecision::AutoAllow),
        (true, PolicyDecision::Deny),
    ] {
        assert!(matches!(
            judge_web_network_access(&role, "TestRole", "web_fetch", policy, enabled),
            NetworkAccessDecision::Deny { .. },
        ));
    }
}

#[test]
fn fetch_always_requires_per_call_review() {
    let role = RoleCapabilities::new(["web_fetch"], false);
    for per_tool in [PolicyDecision::AutoAllow, PolicyDecision::Ask] {
        assert!(matches!(
            judge_web_network_access(&role, "TestRole", "web_fetch", per_tool, true),
            NetworkAccessDecision::Ask { .. },
        ));
    }
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
        if [Role::Orchestrator, Role::WebResearcher, Role::Planner].contains(&role) {
            assert_eq!(policy.authorize("web_fetch"), Ok(()));
        } else {
            assert!(
                matches!(
                    policy.authorize("web_fetch"),
                    Err(RuntimeError::CapabilityDenied { .. }),
                ),
                "{role:?} must not expose web_fetch"
            );
        }
    }
}
