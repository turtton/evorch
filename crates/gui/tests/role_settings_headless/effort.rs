#[test]
fn effort_choices_follow_selected_model_levels() {
    // Given: a config where route "fast" maps to a model with restricted effort levels.
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(
        temp.path().join("evorch.toml"),
        r#"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
default_model = "base"
models = [
  { id = "base", enabled = true },
  { id = "fast", enabled = true, effort_levels = ["minimal", "high"] },
]
[routing.routes]
worker = [{ profile = "local", model = "base" }]
fast = [{ profile = "local", model = "fast" }]
"#,
    )
    .expect("fixture config");
    let (mut harness, _) = super::support::open_fixture(temp.path());
    harness.run();
    // When: the worker role points at the restricted model.
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .worker
        .base
        .logical_model = Some("fast".into());
    harness.run();
    harness.click_label("Worker");
    harness.run();
    harness.click_label("Generation overrides");
    harness.run();
    harness.click_label("Reasoning effort");
    harness.run();
    // Then: only the model's levels are offered, and the default route keeps common levels.
    assert!(harness.has_label("minimal"));
    assert!(harness.has_label("high"));
    assert!(!harness.has_label("xhigh"));
    assert_eq!(
        gui::model::role_settings::effort_options(
            &harness.state().role_settings().effort_choices,
            Some("worker")
        ),
        gui::model::role_settings::DEFAULT_EFFORT_LEVELS
            .map(str::to_owned)
            .into_iter()
            .collect::<Vec<_>>()
    );
}
