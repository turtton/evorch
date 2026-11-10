use agents::RoleCapabilities;
use runtime::scope::{ScopeDecision, ScopeDimension, ScopeLayer, judge_tool_scope};
use sandbox::PolicyDecision;

const DIMENSIONS: [ScopeDimension; 5] = [
    ScopeDimension::Network,
    ScopeDimension::FsRead,
    ScopeDimension::FsWrite,
    ScopeDimension::ProcessSpawn,
    ScopeDimension::Credential,
];

#[test]
fn network_scope_follows_allowlist_and_per_tool_policy() {
    for listed in [false, true] {
        for policy in [
            PolicyDecision::AutoAllow,
            PolicyDecision::Ask,
            PolicyDecision::Deny,
        ] {
            let role = RoleCapabilities::new(listed.then_some("tool"), false);
            let decision =
                judge_tool_scope(&role, "role", "tool", &[ScopeDimension::Network], policy);
            let expected = if !listed {
                Some((false, ScopeLayer::RoleAllowlist))
            } else {
                match policy {
                    PolicyDecision::AutoAllow => None,
                    PolicyDecision::Ask => Some((true, ScopeLayer::PerToolPolicy)),
                    PolicyDecision::Deny => Some((false, ScopeLayer::PerToolPolicy)),
                }
            };
            assert_decision(decision, expected, Some(ScopeDimension::Network));
        }
    }
}

#[test]
fn credential_denies_when_policy_cannot_prove_a_grant() {
    // Given: even AutoAllow cannot establish an explicit credential grant.
    let role = RoleCapabilities::new(["tool"], false);
    for policy in [
        PolicyDecision::AutoAllow,
        PolicyDecision::Ask,
        PolicyDecision::Deny,
    ] {
        // When: credential is requested along with other dimensions.
        let decision = judge_tool_scope(&role, "role", "tool", &DIMENSIONS, policy);
        // Then: the missing role grant is always denied before per-tool policy.
        assert_decision(
            decision,
            Some((false, ScopeLayer::RoleCredential)),
            Some(ScopeDimension::Credential),
        );
    }
}

#[test]
fn local_scope_follows_per_tool_policy() {
    // Given: each local dimension for a tool exposed to the role.
    let role = RoleCapabilities::new(["tool"], false);
    for dimension in [
        ScopeDimension::FsRead,
        ScopeDimension::FsWrite,
        ScopeDimension::ProcessSpawn,
    ] {
        for (policy, expected) in [
            (PolicyDecision::AutoAllow, None),
            (PolicyDecision::Ask, Some((true, ScopeLayer::PerToolPolicy))),
            (
                PolicyDecision::Deny,
                Some((false, ScopeLayer::PerToolPolicy)),
            ),
        ] {
            // When: judging a local-only tool.
            let decision = judge_tool_scope(&role, "role", "tool", &[dimension], policy);
            // Then: policy names the required dimension.
            assert_decision(decision, expected, Some(dimension));
        }
    }
}

#[test]
fn empty_scope_when_tool_wide_gates_apply() {
    // Given: no required dimensions, with independent tool-wide gates.
    for (listed, policy, expected) in [
        (
            false,
            PolicyDecision::AutoAllow,
            Some((false, ScopeLayer::RoleAllowlist)),
        ),
        (
            true,
            PolicyDecision::Deny,
            Some((false, ScopeLayer::PerToolPolicy)),
        ),
        (
            true,
            PolicyDecision::Ask,
            Some((true, ScopeLayer::PerToolPolicy)),
        ),
        (true, PolicyDecision::AutoAllow, None),
    ] {
        let role = RoleCapabilities::new(listed.then_some("tool"), false);
        // When: evaluating the empty scope.
        let decision = judge_tool_scope(&role, "role", "tool", &[], policy);
        // Then: no fabricated dimension and no bypass of tool-wide gates.
        assert_decision(decision, expected, None);
    }
}

#[test]
fn dimension_order_when_scope_contains_duplicates() {
    // Given: reversed, duplicated scope declarations and a policy ask.
    let role = RoleCapabilities::new(["tool"], false);
    let scope = [
        ScopeDimension::FsWrite,
        ScopeDimension::FsRead,
        ScopeDimension::FsWrite,
    ];
    // When: evaluating the scope.
    let decision = judge_tool_scope(&role, "role", "tool", &scope, PolicyDecision::Ask);
    // Then: canonical dimension order is independent of declaration order.
    assert_decision(
        decision,
        Some((true, ScopeLayer::PerToolPolicy)),
        Some(ScopeDimension::FsRead),
    );
}

#[test]
fn reasons_when_names_contain_sensitive_text() {
    // Given: untrusted names in both allowed and denied roles.
    let secret = "secret=value\n/private/policy.toml";
    for listed in [false, true] {
        let role = RoleCapabilities::new(listed.then_some(secret), false);
        for dimension in DIMENSIONS {
            for policy in [PolicyDecision::Ask, PolicyDecision::Deny] {
                // When: producing denial/approval diagnostics.
                let decision = judge_tool_scope(&role, secret, secret, &[dimension], policy);
                // Then: reason is composed solely of typed, stable identifiers.
                match decision {
                    ScopeDecision::Allow => panic!("expected a gate"),
                    ScopeDecision::Deny {
                        layer,
                        dimension,
                        reason,
                    }
                    | ScopeDecision::NeedsApproval {
                        layer,
                        dimension,
                        reason,
                    } => {
                        assert_eq!(
                            reason,
                            format!(
                                "{}.{}",
                                layer.as_str(),
                                dimension.map_or("tool", ScopeDimension::as_str)
                            )
                        );
                        assert!(
                            reason
                                .bytes()
                                .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'.')
                        );
                    }
                }
            }
        }
    }
}

fn assert_decision(
    decision: ScopeDecision,
    expected: Option<(bool, ScopeLayer)>,
    dimension: Option<ScopeDimension>,
) {
    match (decision, expected) {
        (ScopeDecision::Allow, None) => {}
        (
            ScopeDecision::Deny {
                layer,
                dimension: actual,
                ..
            },
            Some((false, expected)),
        )
        | (
            ScopeDecision::NeedsApproval {
                layer,
                dimension: actual,
                ..
            },
            Some((true, expected)),
        ) => {
            assert_eq!((layer, actual), (expected, dimension));
        }
        (actual, expected) => panic!("unexpected decision {actual:?}; expected {expected:?}"),
    }
}
