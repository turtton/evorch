use gui::model::role_settings::{RoleSettingsModel, categories_for_role};

#[test]
fn editor_categories_follow_settings_registry_including_shell_audits() {
    for role in ["worker", "reviewer", "explorer"] {
        let actual: Vec<_> = categories_for_role(role)
            .map(|category| category.name)
            .collect();
        let expected: Vec<_> = config::agent_categories::settings_categories()
            .filter(|category| category.role == role)
            .map(|category| category.name)
            .collect();
        assert_eq!(actual, expected);
        assert!(!actual.contains(&"lesson"));
        assert!(!actual.contains(&"lesson_review"));
    }
    assert_eq!(
        categories_for_role("reviewer")
            .map(|category| category.name)
            .collect::<Vec<_>>(),
        ["plan", "tool-execution"]
    );
}

#[test]
fn reviewer_explicit_category_models_and_efforts_are_seeded() {
    let mut config = config::Config::default();
    let mut model = config::ModelEntryConfig::enabled("category-model");
    model.effort_levels = Some(vec!["custom".into()]);
    config.providers.insert(
        "local".into(),
        config::ProviderProfileConfig {
            models: vec![model],
            ..Default::default()
        },
    );
    config.agents.reviewer.categories.insert(
        "plan".into(),
        config::CategoryBindingConfig {
            logical_model: Some("category-model".into()),
            ..Default::default()
        },
    );
    let editor = RoleSettingsModel::seed_from_config(&config);
    assert_eq!(editor.logical_models, ["category-model"]);
    assert_eq!(editor.effort_choices["category-model"], ["custom"]);
    assert_eq!(editor.agents, config.agents);
    assert!(editor.validate().is_ok());
}

#[test]
fn category_validation_uses_owning_role_and_preserves_internal_bindings() {
    let mut editor = RoleSettingsModel::default();
    for category in ["plan", "tool-execution", "lesson_review"] {
        editor
            .agents
            .reviewer
            .categories
            .insert(category.into(), Default::default());
    }
    editor
        .agents
        .worker
        .categories
        .insert("lesson".into(), Default::default());
    assert!(editor.validate().is_ok());
    editor
        .agents
        .reviewer
        .categories
        .insert("quick".into(), Default::default());
    assert!(editor.validate().is_err());
    editor.agents.reviewer.categories.remove("quick");
    editor
        .agents
        .worker
        .categories
        .insert("plan".into(), Default::default());
    assert!(editor.validate().is_err());
    editor.agents.worker.categories.remove("plan");
    editor
        .agents
        .reviewer
        .categories
        .insert("unknown".into(), Default::default());
    assert!(editor.validate().is_err());
}

#[test]
fn reviewer_category_rejects_blank_models_and_invalid_generation_before_save() {
    for category in ["plan", "tool-execution", "lesson_review"] {
        let mut editor = RoleSettingsModel::default();
        editor.agents.reviewer.categories.insert(
            category.into(),
            config::CategoryBindingConfig {
                logical_model: Some("   ".into()),
                ..Default::default()
            },
        );
        assert!(
            editor
                .validate()
                .unwrap_err()
                .to_string()
                .contains(&format!(
                    "agents.reviewer.categories.{category}.logical_model"
                ))
        );
        let binding = editor.agents.reviewer.categories.get_mut(category).unwrap();
        binding.logical_model = None;
        binding.generation.temperature = Some(3.0);
        assert!(
            editor
                .validate()
                .unwrap_err()
                .to_string()
                .contains(&format!(
                    "agents.reviewer.categories.{category}.generation.temperature"
                ))
        );
        let binding = editor.agents.reviewer.categories.get_mut(category).unwrap();
        binding.generation.temperature = None;
        binding.generation.top_p = Some(-0.1);
        assert!(editor.validate().is_err());
        let binding = editor.agents.reviewer.categories.get_mut(category).unwrap();
        binding.generation.top_p = None;
        binding.generation.max_tokens = Some(0);
        assert!(editor.validate().is_err());
    }
}
