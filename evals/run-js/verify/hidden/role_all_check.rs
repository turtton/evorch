// verify/t4-build-test.sh が検証時だけ crates/agents/tests/ にコピーする隠しテスト。
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
}

#[test]
fn all_round_trips_through_from_name_and_is_unique() {
    let mut seen = BTreeSet::new();
    for role in Role::ALL {
        assert_eq!(Role::from_name(role.name()).unwrap(), role);
        assert!(seen.insert(role.name()), "duplicate role {}", role.name());
    }
}

#[test]
fn all_is_a_const_array_of_nine() {
    const ALL: [Role; 9] = Role::ALL;
    assert_eq!(ALL.len(), 9);
}
