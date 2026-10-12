use super::support::{finish, open_fixture};
use runtime::{AgentModel, Role};

fn write_config(root: &std::path::Path, worker_models: &str) {
    std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).expect("config directory");
    std::fs::write(
        config::project_main_config_path(root),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["base", {{ id = "fast", enabled = true, effort_levels = ["minimal", "high"] }}]
default_model = "base"
[routing.routes]
worker = [{{ profile = "local", model = "fast" }}]
fast = [{{ profile = "local", model = "base" }}]
spare = [{{ profile = "local", model = "base" }}]
[agents.worker]
models = {worker_models}
"#
        ),
    )
    .expect("fixture config");
}

fn saved(root: &std::path::Path) -> config::Config {
    config::Config::load(&config::LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .expect("saved config")
}

// Given: worker のチェーンの先頭 route の実モデルが推論強度を制限している
// When: 先頭要素の推論強度を選んで保存する
// Then: 実モデルの強度だけが選択肢に出て、要素の強度が保存され runtime に反映される
#[test]
fn head_effort_offers_the_route_model_levels_and_saves() {
    let temp = tempfile::tempdir().expect("temp");
    write_config(temp.path(), "[{ route = 'worker' }]");
    let (mut harness, model) = open_fixture(temp.path());
    harness.run();
    harness.click_label("Worker");
    harness.run();
    harness.click_label("Role route reasoning effort");
    harness.run();
    assert!(harness.has_label("minimal"));
    assert!(!harness.has_label("xhigh"));
    harness.click_label("high");
    harness.run();
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);

    assert_eq!(harness.state().role_settings().error, None);
    assert_eq!(
        saved(temp.path()).agents.worker.base.models,
        [config::ChainEntryConfig::with_effort("worker", "high")]
    );
    assert_eq!(
        model
            .selected_reasoning_effort(Role::Worker, None)
            .as_deref(),
        Some("high")
    );
}

// Given: 2 要素のチェーン
// When: 先頭の route を差し替えて保存する
// Then: 継承 (チェーンの削除) は選べず、後続の要素と先頭の強度はそのまま残る
#[test]
fn editing_the_head_keeps_the_fallback_entries() {
    let temp = tempfile::tempdir().expect("temp");
    write_config(
        temp.path(),
        "[{ route = 'worker', reasoning_effort = 'minimal' }, { route = 'spare' }]",
    );
    let (mut harness, _) = open_fixture(temp.path());
    harness.run();
    harness.click_label("Worker");
    harness.run();
    assert!(harness.has_label("Falls back to spare"));
    harness.click_label("Role route");
    harness.run();
    assert!(!harness.has_label("Inherit default"));
    harness.click_label("fast");
    harness.run();
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);

    assert_eq!(harness.state().role_settings().error, None);
    assert_eq!(
        saved(temp.path()).agents.worker.base.models,
        [
            config::ChainEntryConfig::with_effort("fast", "minimal"),
            config::ChainEntryConfig::route("spare"),
        ]
    );
}

// Given: 汎用の推論強度を受け付けない provider だけを候補に持つ route
// When: route の推論強度の選択肢を求める
// Then: 選択肢はなく、他の provider を含む route だけが選択肢を持つ
#[test]
fn native_effort_providers_offer_no_generic_effort() {
    for kind in ["anthropic", "anthropic-subscription", "cursor"] {
        let config: config::Config = toml::from_str(&format!(
            r#"
[providers.target]
type = "{kind}"
models = [{{ id = "selected-model", enabled = true, effort_levels = ["high"] }}]
default_model = "selected-model"
[providers.local]
type = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key_env = "TEST_KEY"
models = ["base"]
default_model = "base"
[routing.routes]
native = [{{ profile = "target" }}]
mixed = [{{ profile = "target" }}, {{ profile = "local" }}]
"#
        ))
        .expect("config");

        assert!(
            gui::model::routing_settings::route_effort_choices(&config, "native").is_empty(),
            "{kind}"
        );
        assert!(
            !gui::model::routing_settings::route_effort_choices(&config, "mixed").is_empty(),
            "{kind}"
        );
    }
}
