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
    assert!(harness.has_label("Role route fallback 1"));
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

// Given: 1 要素のチェーン
// When: 後続の要素を 2 つ追加し、強度を設定し、並べ替えて 1 つ削除して保存する
// Then: 編集した順序と要素ごとの強度が保存され、開き直しても残る
#[test]
fn fallback_entries_can_be_added_reordered_removed_and_keep_their_effort() {
    let temp = tempfile::tempdir().expect("temp");
    write_config(temp.path(), "[{ route = 'worker' }]");
    let (mut harness, _) = open_fixture(temp.path());
    harness.run();
    harness.click_label("Worker");
    harness.run();
    harness.click_label("Add Role route fallback");
    harness.run();
    harness.click_label("Add Role route fallback");
    harness.run();
    harness.click_label("Role route fallback 2 reasoning effort");
    harness.run();
    harness.click_label("low");
    harness.run();
    harness.click_label("Move Role route fallback 2 up");
    harness.run();
    harness.click_label("Remove Role route fallback 2");
    harness.run();
    harness.click_label("Save role settings");
    harness.step();
    finish(&mut harness);

    assert_eq!(harness.state().role_settings().error, None);
    let expected = [
        config::ChainEntryConfig::route("worker"),
        config::ChainEntryConfig::with_effort("spare", "low"),
    ];
    assert_eq!(saved(temp.path()).agents.worker.base.models, expected);
    harness.state_mut().open_role_settings();
    assert_eq!(
        harness.state().role_settings().agents.worker.base.models,
        expected
    );
}

// Given: a planner chain names a route that is not defined yet
// When: a worker fallback entry is picked
// Then: only defined routes are offered, though the head may still show the missing one
#[test]
fn fallback_entries_only_offer_defined_routes() {
    let temp = tempfile::tempdir().expect("temp");
    write_config(temp.path(), "[{ route = 'worker' }, { route = 'spare' }]");
    let path = config::project_main_config_path(temp.path());
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        format!("{text}[agents.roles.planner]\nmodels = [{{ route = 'ghost' }}]\n"),
    )
    .unwrap();
    let (mut harness, _) = open_fixture(temp.path());
    harness.run();
    harness.click_label("Worker");
    harness.run();
    harness.click_label("Role route fallback 1");
    harness.run();
    assert!(harness.has_label("fast"));
    assert!(!harness.has_label("ghost"));
    harness.click_label("Role route");
    harness.run();
    assert!(harness.has_label("ghost"));
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

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_role_chain_editor_png_evidence() {
    // Given: a worker chain with a fallback entry, expanded in the editor.
    let temp = tempfile::tempdir().expect("temp");
    write_config(
        temp.path(),
        "[{ route = 'worker', reasoning_effort = 'minimal' }, { route = 'spare', reasoning_effort = 'low' }]",
    );
    let (mut harness, _) = open_fixture(temp.path());
    harness.run();
    harness.click_label("Worker");
    harness.run();
    // When: capturing the real egui surface offscreen.
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gui-evidence/role-settings");
    std::fs::create_dir_all(&output).expect("evidence directory");
    frame
        .save_png(&output.join("role-chain-editor.png"))
        .expect("PNG");
    // Then: each entry's controls are on screen.
    assert!(harness.has_label("Role route fallback 1"));
    assert!(harness.has_label("Add Role route fallback"));
}
