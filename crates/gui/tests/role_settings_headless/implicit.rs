use super::fixture;

#[test]
fn implicit_roles_without_routes_offer_creation_with_resolved_role_name() {
    for (label, role_key) in [
        ("Orchestrator", "orchestrator"),
        ("Explorer", "explorer"),
        ("Worker", "worker"),
        ("Reviewer", "reviewer"),
        ("Librarian", "librarian"),
        ("Planner", "planner"),
        ("Oracle", "oracle"),
        ("Multimodal Looker", "multimodal_looker"),
    ] {
        // Given: providers but no routes or explicit role bindings.
        let temp = tempfile::tempdir().expect("temp");
        let (mut harness, _) = fixture(temp.path());
        let path = temp.path().join("evorch.toml");
        let text = std::fs::read_to_string(&path).expect("config");
        let text = text.split("[routing.routes]").next().expect("profiles");
        std::fs::write(&path, text).expect("no routes");
        harness.state_mut().open_role_settings();
        let agents = harness.state().role_settings().agents.clone();
        harness.run();
        harness.click_label(label);
        harness.run();
        assert!(harness.has_label("未定義 (route なし)"), "{role_key}");
        assert!(harness.has_label("route を作成"), "{role_key}");
        // When: clicking the missing implicit reference's action.
        harness.click_label("route を作成");
        harness.run();
        // Then: routing is prefilled without materializing an agents binding or writing disk.
        assert!(!harness.state().role_settings().open);
        assert!(harness.state().routing_settings().open);
        assert_eq!(
            harness
                .state()
                .routing_settings()
                .pending_new_route
                .as_deref(),
            Some(role_key)
        );
        assert!(
            harness
                .state()
                .routing_settings()
                .routes
                .contains_key(role_key)
        );
        assert_eq!(harness.state().role_settings().agents, agents);
        assert_eq!(
            std::fs::read_to_string(&path).expect("unchanged config"),
            text
        );
    }
}

#[test]
fn category_status_uses_override_then_worker_binding_then_worker_name() {
    for (worker, category, expected_warnings) in [
        (None, None, 0),                         // implicit worker has a route
        (Some("fast"), None, 0),                 // inherits the explicit worker route
        (None, Some("missing-category"), 1),     // category overrides the routed base
        (Some("missing-base"), Some("fast"), 1), // category overrides the missing base
        (Some("missing-base"), None, 2),         // category inherits the missing base
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut harness, _) = fixture(temp.path());
        let model = harness.state_mut().role_settings_mut();
        model.agents.worker.base.logical_model = worker.map(str::to_owned);
        if let Some(category) = category {
            model.agents.worker.categories.insert(
                "quick".into(),
                config::CategoryBindingConfig {
                    logical_model: Some(category.into()),
                    ..Default::default()
                },
            );
        }
        let before = model.agents.clone();
        harness.run();
        harness.click_label("Worker");
        harness.run();
        harness.click_label("quick");
        harness.run();
        assert_eq!(
            harness.count_labels("未定義 (route なし)"),
            expected_warnings,
            "worker={worker:?}, category={category:?}"
        );
        assert_eq!(harness.count_labels("route を作成"), expected_warnings);
        assert_eq!(harness.state().role_settings().agents, before);
        if category == Some("missing-category") {
            harness.click_label("route を作成");
            harness.run();
            assert_eq!(
                harness
                    .state()
                    .routing_settings()
                    .pending_new_route
                    .as_deref(),
                Some("missing-category")
            );
        }
    }
}

#[test]
fn implicit_category_without_worker_route_warns_without_creating_an_override() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = fixture(temp.path());
    let model = harness.state_mut().role_settings_mut();
    // A different route still exists; there is no fallback to its provider.
    model.route_names.remove("worker");
    assert!(!model.route_names.is_empty());
    harness.run();
    harness.click_label("Worker");
    harness.run();
    assert_eq!(harness.count_labels("未定義 (route なし)"), 1);
    harness.click_label("quick");
    harness.run();
    assert_eq!(harness.count_labels("未定義 (route なし)"), 2);
    assert_eq!(harness.count_labels("route を作成"), 2);
    assert_eq!(
        harness.state().role_settings().agents.worker.logical_model,
        None
    );
    assert!(
        harness
            .state()
            .role_settings()
            .agents
            .worker
            .categories
            .is_empty()
    );
}
