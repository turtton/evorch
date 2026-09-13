use agents::{NetworkAccess, RoleCapabilities};
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
fn network_decision_when_layers_vary() {
    // Given: all combinations of allowlist, role, policy and session grants.
    for listed in [false, true] {
        for role_network in [
            NetworkAccess::Denied,
            NetworkAccess::OptIn,
            NetworkAccess::Allowed,
        ] {
            for policy in [
                PolicyDecision::AutoAllow,
                PolicyDecision::Ask,
                PolicyDecision::Deny,
            ] {
                for session in [
                    NetworkAccess::Denied,
                    NetworkAccess::OptIn,
                    NetworkAccess::Allowed,
                ] {
                    let role = RoleCapabilities::new(listed.then_some("tool"), role_network, false);
                    // When: judging the network scope.
                    let decision = judge_tool_scope(
                        &role,
                        "role",
                        "tool",
                        &[ScopeDimension::Network],
                        policy,
                        session,
                    );
                    // Then: first deny wins, otherwise first ask, otherwise allow.
                    let expected = if !listed {
                        Some((false, ScopeLayer::RoleAllowlist))
                    } else {
                        match (role_network, policy, session) {
                            (NetworkAccess::Denied, _, _) => Some((false, ScopeLayer::RoleNetwork)),
                            (_, PolicyDecision::Deny, _) => {
                                Some((false, ScopeLayer::PerToolPolicy))
                            }
                            (_, _, NetworkAccess::Denied) => Some((false, ScopeLayer::Session)),
                            (_, PolicyDecision::Ask, _) => Some((true, ScopeLayer::PerToolPolicy)),
                            (_, _, NetworkAccess::OptIn) => Some((true, ScopeLayer::Session)),
                            (_, PolicyDecision::AutoAllow, NetworkAccess::Allowed) => None,
                        }
                    };
                    assert_decision(decision, expected, Some(ScopeDimension::Network));
                }
            }
        }
    }
}

#[test]
fn credential_denies_when_policy_cannot_prove_a_grant() {
    // Given: even AutoAllow cannot establish an explicit credential grant.
    let role = RoleCapabilities::new(["tool"], NetworkAccess::Allowed, false);
    for policy in [
        PolicyDecision::AutoAllow,
        PolicyDecision::Ask,
        PolicyDecision::Deny,
    ] {
        // When: credential is requested along with other dimensions.
        let decision = judge_tool_scope(
            &role,
            "role",
            "tool",
            &DIMENSIONS,
            policy,
            NetworkAccess::Allowed,
        );
        // Then: the missing role grant is always denied before per-tool policy.
        assert_decision(
            decision,
            Some((false, ScopeLayer::RoleCredential)),
            Some(ScopeDimension::Credential),
        );
    }
}

#[test]
fn local_scope_when_network_is_denied() {
    // Given: each local dimension and a role/session denying only network.
    let role = RoleCapabilities::new(["tool"], NetworkAccess::Denied, false);
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
            let decision = judge_tool_scope(
                &role,
                "role",
                "tool",
                &[dimension],
                policy,
                NetworkAccess::Denied,
            );
            // Then: policy names the required dimension; network grants are irrelevant.
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
        let role = RoleCapabilities::new(listed.then_some("tool"), NetworkAccess::Denied, false);
        // When: evaluating the empty scope.
        let decision = judge_tool_scope(&role, "role", "tool", &[], policy, NetworkAccess::Denied);
        // Then: no fabricated dimension and no bypass of tool-wide gates.
        assert_decision(decision, expected, None);
    }
}

#[test]
fn dimension_order_when_scope_contains_duplicates() {
    // Given: reversed, duplicated scope declarations and a policy ask.
    let role = RoleCapabilities::new(["tool"], NetworkAccess::Allowed, false);
    let scope = [
        ScopeDimension::FsWrite,
        ScopeDimension::FsRead,
        ScopeDimension::FsWrite,
    ];
    // When: evaluating the scope.
    let decision = judge_tool_scope(
        &role,
        "role",
        "tool",
        &scope,
        PolicyDecision::Ask,
        NetworkAccess::Allowed,
    );
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
        let role = RoleCapabilities::new(listed.then_some(secret), NetworkAccess::Allowed, false);
        for dimension in DIMENSIONS {
            for policy in [PolicyDecision::Ask, PolicyDecision::Deny] {
                // When: producing denial/approval diagnostics.
                let decision = judge_tool_scope(
                    &role,
                    secret,
                    secret,
                    &[dimension],
                    policy,
                    NetworkAccess::OptIn,
                );
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
