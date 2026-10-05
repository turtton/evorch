use super::{finish, fixture};
use gui::headless::HeadlessWorkbench;

#[test]
fn candidate_reasoning_effort_offers_model_levels_and_saves() {
    // Given: a route whose candidate model restricts its effort levels.
    let temp = tempfile::tempdir().expect("temp");
    let path = config::project_main_config_path(temp.path());
    let (mut state, runtime) = fixture(temp.path());
    let text = std::fs::read_to_string(&path).expect("config");
    std::fs::write(
        &path,
        text.replace(
            "models = [\"base\", \"fast\"]",
            "models = [\"base\", { id = \"fast\", enabled = true, effort_levels = [\"minimal\", \"high\"] }]",
        ) + "\n[routing.routes]\nworker = [{ profile = 'local', model = 'fast' }]\n",
    )
    .expect("route");
    state.open_routing_settings();
    state
        .routing_settings_mut()
        .expanded
        .insert("worker".into());
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // When: opening the candidate effort picker and choosing a level.
    harness.click_label("worker candidate 1 reasoning effort");
    harness.run();
    assert!(harness.has_label("minimal"));
    assert!(!harness.has_label("xhigh"));
    harness.click_label("high");
    harness.run();
    harness.click_label("Save routing");
    finish(&mut harness);
    // Then: the candidate effort is persisted and the reloaded runtime reports it.
    let saved = config::Config::load(&config::LoadOptions {
        project_dir: Some(temp.path().into()),
        user_config_dir: Some(temp.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("saved config");
    assert_eq!(
        saved.routing.routes["worker"][0]
            .reasoning_effort
            .as_deref(),
        Some("high")
    );
    assert_eq!(
        runtime::AgentModel::selected_reasoning_effort(
            runtime.as_ref(),
            runtime::Role::Worker,
            None
        )
        .as_deref(),
        Some("high")
    );
}
