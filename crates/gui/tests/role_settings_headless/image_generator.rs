//! The Image generator chain and kind-filtered route choices in role settings (ADR 0038).

use super::support::{finish, open_fixture};

fn image_fixture(
    root: &std::path::Path,
) -> (
    gui::headless::HeadlessWorkbench<gui::fixture::DemoSource>,
    std::sync::Arc<runtime::compose::SwitchableModel>,
) {
    std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).expect("config directory");
    std::fs::write(
        config::project_main_config_path(root),
        r#"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["gpt-5"]
default_model = "gpt-5"
[providers.codex]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "https://example.invalid/codex"
credential = { type = "keyring", service = "evorch", account = "codex" }
models = ["gpt-6-astra", "gpt-image-1"]
default_model = "gpt-6-astra"
[routing.routes]
worker = [{ profile = "local" }]
chat = [{ profile = "local" }]
art = [{ profile = "codex", model = "gpt-image-1" }]
"#,
    )
    .expect("fixture config");
    open_fixture(root)
}

fn saved(root: &std::path::Path) -> config::AgentsConfig {
    config::Config::load(&config::LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .expect("saved config")
    .agents
}

#[test]
fn image_generator_offers_only_image_routes_and_saves_its_chain() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = image_fixture(temp.path());
    harness.run();
    harness.click_label("Image generator");
    harness.run();
    // Given: an empty chain means image generation is off.
    assert_eq!(
        harness.label_value("Image generator route").as_deref(),
        Some(gui::panes::role_settings::IMAGE_GENERATION_DISABLED)
    );
    // When: the route picker opens.
    harness.click_label("Image generator route");
    harness.run();
    // Then: only the image route is offered.
    assert!(harness.has_label("art"));
    assert!(!harness.has_label("chat"));
    assert!(!harness.has_label("worker"));
    harness.click_label("art");
    harness.run();
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);

    assert_eq!(harness.state().role_settings().error, None);
    let agents = saved(temp.path());
    assert_eq!(
        agents.roles.image_generator.models,
        [config::ChainEntryConfig::route("art")]
    );
    assert!(agents.roles.image_generator.generation == Default::default());
}

#[test]
fn agent_roles_never_offer_image_routes() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = image_fixture(temp.path());
    harness.run();
    harness.click_label("Planner");
    harness.run();
    // When
    harness.click_label("Role route");
    harness.run();
    // Then: chat routes are offered, the image route is not.
    assert!(harness.has_label("chat"));
    assert!(!harness.has_label("art"));
}

#[test]
fn a_saved_image_route_in_a_role_chain_stays_visible_and_blocks_saving() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _) = image_fixture(temp.path());
    let path = config::project_main_config_path(temp.path());
    let before = std::fs::read(&path).expect("config");
    // Given: the planner's chain names the image route, as a hand-edited config could.
    harness
        .state_mut()
        .role_settings_mut()
        .agents
        .roles
        .planner
        .models = vec![config::ChainEntryConfig::route("art")];
    harness.run();
    harness.click_label("Planner");
    harness.run();
    // Then: the entry stays selected and is flagged.
    assert_eq!(harness.label_value("Role route").as_deref(), Some("art"));
    assert!(harness.has_label(
        "Route `art` has models of another kind. Only routes of chat models are offered for agent roles and the compactor."
    ));
    // When: saving.
    harness.click_label("Save role settings");
    harness.run();
    // Then: the kind check refuses it before writing.
    let settings = harness.state().role_settings();
    let error = settings.error.as_deref().expect("validation error");
    assert!(error.contains("agents.roles.planner.models"), "{error}");
    assert!(error.contains("codex/gpt-image-1"), "{error}");
    assert!(settings.open && !settings.is_saving());
    assert_eq!(std::fs::read(&path).expect("config"), before);
}
