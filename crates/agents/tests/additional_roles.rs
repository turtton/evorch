use agents::Role;

#[test]
fn additional_roles_have_least_privilege_capabilities() {
    // Given: the closed role names and their capability contracts.
    let cases = [
        (
            "WebResearcher",
            ["read", "grep", "web_search", "web_fetch"]
                .into_iter()
                .chain(agents::BROWSER_TOOLS)
                .collect::<Vec<_>>(),
        ),
        (
            "Planner",
            vec!["read", "grep", "git_diff", "skill_load", "web_fetch"],
        ),
        ("Advisor", vec!["read", "grep", "git_diff"]),
        ("MultimodalLooker", vec!["read"]),
        (
            "Evaluator",
            ["read", "grep"]
                .into_iter()
                .chain(agents::EVALUATOR_TOOLS)
                .collect::<Vec<_>>(),
        ),
    ];
    for (name, tools) in cases {
        // When: a role is parsed at the boundary.
        let role = Role::from_name(name).expect("known role");
        let capabilities = role.capabilities();
        // Then: its identity round-trips and its authority is exact.
        assert_eq!(role.name(), name);
        assert_eq!(
            capabilities.allowed_tools,
            tools
                .into_iter()
                .chain([
                    "working_note_write",
                    "working_note_read",
                    "history_read",
                    "thread_search",
                    "thread_grep",
                    "thread_read",
                    "ask_user",
                    "user_answers"
                ])
                .map(String::from)
                .collect()
        );
        assert!(!capabilities.can_delegate);
    }
}

#[test]
fn every_role_authorizes_scoped_memory_meta_ops() {
    // Given: all roles, including read-only roles.
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
        Role::WebResearcher,
        Role::Planner,
        Role::Advisor,
        Role::MultimodalLooker,
        Role::Evaluator,
    ] {
        let capabilities = role.capabilities();
        // When / Then: self-scoped memory, question and read-only thread operations are authorized.
        for op in [
            "working_note_write",
            "working_note_read",
            "history_read",
            "thread_search",
            "thread_grep",
            "thread_read",
            "ask_user",
            "user_answers",
        ] {
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
        Role::WebResearcher,
    ] {
        // Given / When: an existing canonical name is parsed.
        let parsed = Role::from_name(role.name()).expect("existing role");
        // Then: no identity changes.
        assert_eq!(parsed, role);
    }
}

#[test]
fn legacy_oracle_names_normalize_to_advisor() {
    for name in ["Oracle", "oracle", "Advisor", "advisor"] {
        assert_eq!(Role::from_name(name), Ok(Role::Advisor));
    }
}

#[test]
fn evaluator_cannot_change_files_run_commands_delegate_or_browse() {
    // Given: the Evaluator's capability boundary (ADR 0036 §1).
    let capabilities = Role::Evaluator.capabilities();
    // When / Then: every mutation, shell, delegation and web tool is denied.
    for tool in [
        "write",
        "edit",
        "apply_patch",
        "shell",
        "git_diff",
        "delegate",
        "web_search",
        "web_fetch",
        "browser_navigate",
    ] {
        assert!(
            matches!(
                capabilities.check_tool(Role::Evaluator.name(), tool),
                agents::CapabilityDecision::Denied { .. }
            ),
            "{tool}"
        );
    }
}
