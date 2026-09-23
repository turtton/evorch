use std::path::Path;

use config::types::agents::rename_logical_model_refs;
use config::{
    AgentsConfig, CategoryBindingConfig, Config, ConfigError, GenerationOverridesConfig,
    LoadOptions, RoleBindingConfig, RouteCandidateConfig, RoutingConfig, save_routing_and_agents,
};

const UNRELATED_PREFIX: &str = "# keep header\nversion = 2 # keep version\n\n[providers.local]\n# keep provider\ntype = 'openai-compatible'\nbase_url = 'https://example.invalid/v1'\napi_key_env = 'KEY'\nmodels = ['base']\ndefault_model = 'base' # keep default\n";
const UNRELATED_SUFFIX: &str = "\n# keep panel\n[panel]\nlayout = 'compact' # keep layout\n";
const OLD_SECTIONS: &str = r#"
# original route
[[routing.routes.old]]
profile = "local"

[agents.explorer]
logical_model = "old"

[agents.worker.categories.quick]
logical_model = "old"

[agents.worker.categories.deep]
preset = "deep-preset"
"#;

fn load(root: &Path) -> Config {
    Config::load(&LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join("user-empty")),
        read_env: false,
        ..Default::default()
    })
    .expect("保存結果を読み込める")
}

fn write_fixture(path: &Path) {
    std::fs::write(
        path,
        format!("{UNRELATED_PREFIX}{OLD_SECTIONS}{UNRELATED_SUFFIX}"),
    )
    .expect("fixture");
}

fn assert_unrelated_comments_preserved(path: &Path) {
    let text = std::fs::read_to_string(path).expect("saved text");
    assert!(text.starts_with(UNRELATED_PREFIX));
    assert!(text.contains(UNRELATED_SUFFIX));
}

fn renamed_sections(config: &Config) -> (RoutingConfig, AgentsConfig) {
    let mut routing = config.routing.clone();
    let candidates = routing.routes.remove("old").expect("old route");
    routing.routes.insert("new".into(), candidates);
    let mut agents = config.agents.clone();
    rename_logical_model_refs(&mut agents, &[("old".into(), "new".into())].into());
    (routing, agents)
}

#[test]
fn roundtrip_preserves_both_sections_and_unrelated_comments() {
    // Given: コメント付き他セクションと、置換前の routing / agents。
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("evorch.toml");
    write_fixture(&path);
    let mut expected = load(temp.path());
    let routing = RoutingConfig {
        routes: [
            (
                "worker.v2/日本語".into(),
                vec![
                    RouteCandidateConfig {
                        profile: "local".into(),
                        model: Some("custom-model".into()),
                    },
                    RouteCandidateConfig {
                        profile: "local".into(),
                        model: None,
                    },
                ],
            ),
            ("empty".into(), vec![]),
        ]
        .into(),
    };
    let binding = RoleBindingConfig {
        logical_model: Some("worker.v2/日本語".into()),
        preset: Some("role-preset".into()),
        generation: GenerationOverridesConfig {
            temperature: Some(0.2),
            top_p: Some(0.9),
            max_tokens: Some(4096),
            reasoning_effort: Some("xhigh".into()),
        },
    };
    let mut agents = AgentsConfig::default();
    for role in [
        &mut agents.orchestrator,
        &mut agents.explorer,
        &mut agents.worker.base,
        &mut agents.reviewer,
        &mut agents.roles.web_researcher,
        &mut agents.roles.planner,
        &mut agents.roles.oracle,
        &mut agents.roles.multimodal_looker,
    ] {
        *role = binding.clone();
    }
    agents.worker.categories.insert(
        "quick".into(),
        CategoryBindingConfig {
            logical_model: binding.logical_model.clone(),
            preset: Some("category-preset".into()),
            generation: binding.generation.clone(),
        },
    );
    agents
        .worker
        .categories
        .insert("deep".into(), CategoryBindingConfig::default());

    // When: 全ロール・カテゴリと順序付きルート候補を一括保存する。
    save_routing_and_agents(&path, &routing, &agents).expect("combined save");

    // Then: 両セクションが一致し、他セクションの値・TOML 表現は維持する。
    expected.routing = routing;
    expected.agents = agents;
    assert_eq!(load(temp.path()), expected);
    assert_unrelated_comments_preserved(&path);
}

#[test]
fn empty_sections_replace_old_routes_and_bindings() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("evorch.toml");
    write_fixture(&path);
    let mut expected = load(temp.path());
    expected.routing = RoutingConfig::default();
    expected.agents = AgentsConfig::default();

    save_routing_and_agents(&path, &expected.routing, &expected.agents).expect("empty save");

    assert_eq!(load(temp.path()), expected);
    assert_unrelated_comments_preserved(&path);
}

#[test]
fn rename_saves_route_and_explicit_refs_together_without_materializing_implicit_refs() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("evorch.toml");
    write_fixture(&path);
    let mut expected = load(temp.path());
    let (routing, agents) = renamed_sections(&expected);

    save_routing_and_agents(&path, &routing, &agents).expect("rename save");

    let loaded = load(temp.path());
    assert!(loaded.routing.routes.contains_key("new"));
    assert!(!loaded.routing.routes.contains_key("old"));
    assert_eq!(loaded.agents.explorer.logical_model.as_deref(), Some("new"));
    assert_eq!(
        loaded.agents.worker.categories["quick"]
            .logical_model
            .as_deref(),
        Some("new")
    );
    assert_eq!(loaded.agents.worker.base.logical_model, None);
    assert_eq!(loaded.agents.worker.categories["deep"].logical_model, None);
    expected.routing = routing;
    expected.agents = agents;
    assert_eq!(loaded, expected);
    assert_unrelated_comments_preserved(&path);
}

#[test]
fn invalid_agents_leave_both_sections_and_file_bytes_unchanged() {
    // Given: リネーム対象の設定と、不正なカテゴリを含む更新後の agents。
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("evorch.toml");
    write_fixture(&path);
    let before = std::fs::read(&path).expect("original bytes");
    let original = load(temp.path());
    let (routing, mut agents) = renamed_sections(&original);
    agents
        .worker
        .categories
        .insert("unknown".into(), CategoryBindingConfig::default());

    // When: routing 単独なら保存可能だが、agents の検証に失敗する。
    let error = save_routing_and_agents(&path, &routing, &agents).expect_err("invalid category");

    // Then: routing も保存されず、ファイル全体がバイト単位で不変。
    assert!(matches!(
        error,
        ConfigError::InvalidField { path, .. } if path == "agents.worker.categories.unknown"
    ));
    assert_eq!(std::fs::read(&path).expect("unchanged bytes"), before);
    assert_eq!(load(temp.path()), original);
    assert!(!temp.path().join("evorch.toml.tmp").exists());
}

#[test]
fn combined_save_creates_new_config_file() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("evorch.toml");
    let original: Config = toml::from_str(OLD_SECTIONS).expect("sections fixture");
    let (routing, agents) = renamed_sections(&original);

    save_routing_and_agents(&path, &routing, &agents).expect("create config");

    let loaded = load(temp.path());
    assert_eq!(loaded.version, config::CURRENT_VERSION);
    assert_eq!(loaded.routing, routing);
    assert_eq!(loaded.agents, agents);
}
