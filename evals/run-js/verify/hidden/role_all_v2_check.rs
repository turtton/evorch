// verify/t4-build-test-v2.sh が検証時だけ crates/agents/tests/ にコピーする隠しテスト。
use agents::Role;
use std::collections::BTreeSet;

#[test]
fn all_lists_every_role_in_declaration_order() {
    let names: Vec<&str> = Role::ALL.iter().map(|role| role.name()).collect();
    assert_eq!(
        names,
        [
            "Orchestrator",
            "Explorer",
            "Worker",
            "Reviewer",
            "WebResearcher",
            "Planner",
            "Advisor",
            "MultimodalLooker",
            "Evaluator",
        ]
    );
    const ALL: [Role; 9] = Role::ALL;
    assert_eq!(ALL.len(), 9);
}

#[test]
fn explorer_gains_exactly_git_diff_and_skill_load() {
    let tools = Role::Explorer.capabilities().allowed_tools;
    for tool in ["read", "grep", "git_diff", "skill_load"] {
        assert!(tools.contains(tool), "Explorer should allow {tool}");
    }
    for tool in ["write", "edit", "apply_patch", "shell", "delegate", "send", "web_fetch"] {
        assert!(!tools.contains(tool), "Explorer must still deny {tool}");
    }
    let others: BTreeSet<_> = Role::Reviewer.capabilities().allowed_tools;
    assert!(others.contains("git_diff") && !others.contains("skill_load"));
    assert!(!Role::Advisor.capabilities().allowed_tools.contains("skill_load"));
}

#[test]
fn roles_allowing_follows_declaration_order() {
    assert_eq!(
        Role::roles_allowing("skill_load"),
        [Role::Orchestrator, Role::Explorer, Role::Worker, Role::Planner]
    );
    assert_eq!(
        Role::roles_allowing("git_diff"),
        [
            Role::Orchestrator,
            Role::Explorer,
            Role::Worker,
            Role::Reviewer,
            Role::Planner,
            Role::Advisor
        ]
    );
    assert_eq!(Role::roles_allowing("web_search"), [Role::WebResearcher]);
    assert_eq!(Role::roles_allowing("read").len(), 9);
    assert!(Role::roles_allowing("").is_empty());
    assert!(Role::roles_allowing("no_such_tool").is_empty());
}
