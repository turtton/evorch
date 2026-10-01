#[path = "role_settings_headless/support.rs"]
mod support;

use runtime::{AgentModel, Role};
use support::{finish, fixture};

#[test]
fn categories_are_visible_only_on_their_own_role() {
    for role in ["Worker", "Reviewer", "Explorer", "WebResearcher"] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut harness, _) = fixture(temp.path());
        harness.run();
        harness.click_label(role);
        harness.run();
        assert_eq!(
            harness.has_label("Category overrides"),
            matches!(role, "Worker" | "Reviewer")
        );
        for category in [
            "quick",
            "deep",
            "high-reasoning",
            "visual",
            "writing",
            "research",
        ] {
            assert_eq!(
                harness.has_label(category),
                role == "Worker",
                "{role}: {category}"
            );
        }
        for category in ["plan", "tool-execution"] {
            assert_eq!(
                harness.has_label(category),
                role == "Reviewer",
                "{role}: {category}"
            );
        }
        for category in ["lesson", "lesson_review"] {
            assert!(!harness.has_label(category));
        }
    }
}

#[test]
fn reviewer_category_edit_saves_reloads_and_previews_each_route() {
    for category in ["plan", "tool-execution"] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut harness, model) = fixture(temp.path());
        harness
            .state_mut()
            .role_settings_mut()
            .agents
            .reviewer
            .logical_model = Some("worker".into());
        harness.run();
        harness.click_label("Reviewer");
        harness.run();
        harness.click_label(category);
        harness.run();
        harness.click_label(&format!("{category} logical model"));
        harness.run();
        harness.click_label("fast");
        harness.run();
        harness.click_label("Save role settings");
        harness.step();
        finish(&mut harness);
        assert_eq!(harness.state().role_settings().error, None);
        let saved = config::Config::load(&config::LoadOptions {
            project_dir: Some(temp.path().into()),
            user_config_dir: Some(temp.path().join("user")),
            read_env: false,
            ..Default::default()
        })
        .expect("saved config");
        assert_eq!(
            saved.agents.reviewer.categories[category]
                .logical_model
                .as_deref(),
            Some("fast")
        );
        assert!(saved.agents.worker.categories.is_empty());
        assert_eq!(
            model.selected_model(Role::Reviewer, Some(category)),
            "accelerated/fast"
        );
        assert_eq!(model.selected_model(Role::Reviewer, None), "local/base");
        harness.state_mut().open_role_settings();
        harness.run();
        let previews = &harness.state().role_settings().resolved_previews;
        assert_eq!(
            previews[&format!("reviewer.categories.{category}")].as_deref(),
            Some("accelerated/fast")
        );
        assert_eq!(
            previews["worker.categories.quick"].as_deref(),
            Some("local/base")
        );
        assert!(harness.has_label("→ accelerated/fast"));
        harness.click_label("Reset category");
        harness.run();
        harness.click_label("Save role settings");
        harness.step();
        finish(&mut harness);
        assert_eq!(harness.state().role_settings().error, None);
        assert!(
            !harness
                .state()
                .role_settings()
                .agents
                .reviewer
                .categories
                .contains_key(category)
        );
        assert_eq!(
            model.selected_model(Role::Reviewer, Some(category)),
            "local/base"
        );
    }
}

#[test]
fn reviewer_categories_inherit_role_status_without_materializing_overrides() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .reviewer
        .logical_model = Some("fast".into());
    harness.run();
    harness.click_label("Reviewer");
    harness.run();
    harness.click_label("plan");
    harness.run();
    assert!(!harness.has_label("未定義 (route なし)"));
    assert!(
        harness
            .state()
            .role_settings()
            .agents
            .reviewer
            .categories
            .is_empty()
    );
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .reviewer
        .categories
        .insert(
            "plan".into(),
            config::CategoryBindingConfig {
                logical_model: Some("missing-category".into()),
                ..Default::default()
            },
        );
    harness.run();
    assert!(harness.has_label("未定義 (route なし)"));
    harness.click_label("route を作成");
    harness.run();
    assert!(harness.state().routing_settings().open);
    assert!(
        harness
            .state()
            .routing_settings()
            .routes
            .contains_key("missing-category")
    );
}

#[test]
fn internal_bindings_survive_saving_public_reviewer_categories() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    let internal = config::CategoryBindingConfig {
        logical_model: Some("fast".into()),
        generation: config::GenerationOverridesConfig {
            max_tokens: Some(123),
            ..Default::default()
        },
        ..Default::default()
    };
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .reviewer
        .categories
        .insert("lesson_review".into(), internal.clone());
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .worker
        .categories
        .insert("lesson".into(), internal.clone());
    harness.state_mut().submit_role_settings();
    finish(&mut harness);
    assert_eq!(harness.state().role_settings().error, None);
    harness.state_mut().open_role_settings();
    assert_eq!(
        harness.state().role_settings().agents.reviewer.categories["lesson_review"],
        internal
    );
    assert_eq!(
        harness.state().role_settings().agents.worker.categories["lesson"],
        internal
    );
}
