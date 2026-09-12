use agents::{NetworkAccess, Role};

#[test]
fn additional_roles_have_least_privilege_capabilities() {
    // Given: the closed role names and their capability contracts.
    let cases = [
        (
            "Planner",
            vec!["read", "grep", "git_diff", "skill_load", "web_fetch"],
            NetworkAccess::OptIn,
        ),
        (
            "Oracle",
            vec!["read", "grep", "git_diff"],
            NetworkAccess::Denied,
        ),
        ("MultimodalLooker", vec!["read"], NetworkAccess::Denied),
    ];
    for (name, tools, network) in cases {
        // When: a role is parsed at the boundary.
        let role = Role::from_name(name).expect("known role");
        let capabilities = role.capabilities();
        // Then: its identity round-trips and its authority is exact.
        assert_eq!(role.name(), name);
        assert_eq!(
            capabilities.allowed_tools,
            tools
                .into_iter()
                .chain(["ledger_append", "ledger_read"])
                .map(String::from)
                .collect()
        );
        assert_eq!(capabilities.network, network);
        assert!(!capabilities.can_delegate);
    }
}

#[test]
fn every_role_authorizes_ledger_meta_ops() {
    // Given: all roles, including read-only roles.
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
        Role::Librarian,
        Role::Planner,
        Role::Oracle,
        Role::MultimodalLooker,
    ] {
        let capabilities = role.capabilities();
        // When / Then: both self-scoped operations are authorized.
        for op in ["ledger_append", "ledger_read"] {
            assert_eq!(
                capabilities.check_tool(role.name(), op),
                agents::CapabilityDecision::Allowed
            );
        }
    }
}

#[test]
fn unknown_role_is_rejected_instead_of_becoming_worker() {
    // Given / When: an unknown role name crosses the boundary.
    let error = Role::from_name("UnknownRole").expect_err("unknown role");
    // Then: the error identifies the invalid input.
    assert_eq!(error.name, "UnknownRole");
}

#[test]
fn existing_role_names_still_round_trip() {
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
        Role::Librarian,
    ] {
        // Given / When: an existing canonical name is parsed.
        let parsed = Role::from_name(role.name()).expect("existing role");
        // Then: no identity changes.
        assert_eq!(parsed, role);
    }
}
