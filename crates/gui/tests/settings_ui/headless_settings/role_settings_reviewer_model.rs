use gui::model::role_settings::categories_for_role;

#[test]
fn editor_categories_follow_settings_registry_including_shell_audits() {
    for role in ["worker", "reviewer", "explorer"] {
        let actual: Vec<_> = categories_for_role(role)
            .map(|category| category.id.as_str())
            .collect();
        let expected: Vec<_> = config::agent_categories::settings_categories()
            .filter(|category| category.role == role)
            .map(|category| category.id.as_str())
            .collect();
        assert_eq!(actual, expected);
        assert!(!actual.contains(&"lesson"));
        assert!(!actual.contains(&"lesson_review"));
    }
    assert_eq!(
        categories_for_role("reviewer")
            .map(|category| category.id.as_str())
            .collect::<Vec<_>>(),
        ["plan-review", "tool-execution"]
    );
}
