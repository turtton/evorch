//! ロールのモデルチェーン (設定 v3、ADR 0030) の読み込み・保存・移行。

use config::{ChainEntryConfig, Config, ConfigError, LoadOptions};

fn options(root: &std::path::Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(root.to_path_buf()),
        read_env: false,
        ..Default::default()
    }
}

// Given: route と、要素ごとに推論強度を持つ 2 要素のチェーン
// When: 読み込む
// Then: カテゴリ未指定のカテゴリはロールのチェーンを継承し、推論強度も要素ごとに残る
#[test]
fn chain_entries_keep_order_and_per_entry_effort() {
    let tmp = tempfile::tempdir().expect("temp");
    std::fs::write(
        tmp.path().join("config.toml"),
        r#"version = 3
[[routing.routes."moonshotai/kimi-k3"]]
profile = "kimi"
model = "k3"
[[routing.routes."moonshotai/kimi-k3"]]
profile = "neuralwatt"
model = "kimi-k3"
auto = true
[[routing.routes.claude]]
profile = "claude"
[agents.worker]
models = [
  { route = "moonshotai/kimi-k3", reasoning_effort = "high" },
  { route = "claude", reasoning_effort = "medium" },
]
"#,
    )
    .expect("config");

    let config = Config::load_strict(&options(tmp.path())).expect("load");
    let binding = config.agents.binding_for("worker", Some("quick")).unwrap();

    assert_eq!(
        binding.models,
        [
            ChainEntryConfig::with_effort("moonshotai/kimi-k3", "high"),
            ChainEntryConfig::with_effort("claude", "medium"),
        ]
    );
    assert!(config.routing.routes["moonshotai/kimi-k3"][1].auto);
}

// Given: 存在しない route を参照するチェーン
// When: 厳格に読み込む / 通常に読み込む
// Then: 厳格な読み込みは参照元のパス付きで失敗し、通常の読み込みは続行する
#[test]
fn unknown_route_reference_is_rejected_by_strict_load() {
    let tmp = tempfile::tempdir().expect("temp");
    std::fs::write(
        tmp.path().join("config.toml"),
        "[[routing.routes.claude]]\nprofile = 'claude'\n\
         [agents.roles.planner]\nmodels = [{ route = 'claude' }, { route = 'missing' }]\n",
    )
    .expect("config");

    let error = Config::load_strict(&options(tmp.path())).expect_err("unknown route");
    assert!(
        matches!(&error, ConfigError::InvalidField { path, message }
            if path == "agents.roles.planner.models" && message.contains("`missing`")),
        "{error}"
    );
    let tolerant = Config::load(&options(tmp.path())).expect("tolerant load continues");
    assert_eq!(tolerant.agents.roles.planner.models.len(), 2);
}

// Given: チェーンの要素に未知のキー
// When: 厳格に読み込む
// Then: 要素の位置を含むパスで拒否される
#[test]
fn unknown_chain_entry_field_reports_indexed_path() {
    let tmp = tempfile::tempdir().expect("temp");
    std::fs::write(
        tmp.path().join("config.toml"),
        "[[routing.routes.claude]]\nprofile = 'claude'\n\
         [agents.worker.categories.deep]\nmodels = [{ route = 'claude', weight = 2 }]\n",
    )
    .expect("config");

    let error = Config::load_strict(&options(tmp.path())).expect_err("unknown key");

    assert!(
        error
            .to_string()
            .contains("agents.worker.categories.deep.models[0].weight"),
        "{error}"
    );
}

// Given: v2 のファイル (logical_model と route 候補の reasoning_effort、コメント付きの別セクション)
// When: 別セクションを保存する
// Then: ファイルは v3 になり、推論強度がチェーンへ移り、無関係なコメントは残る
#[test]
fn saving_into_v2_file_upgrades_it_in_place() {
    let tmp = tempfile::tempdir().expect("temp");
    let path = tmp.path().join("config.toml");
    std::fs::write(
        &path,
        "version = 2\n# keep panel\n[panel]\nlayout = 'compact'\n\n\
         [[routing.routes.sol]]\nprofile = 'codex'\nreasoning_effort = 'xhigh'\n\n\
         [agents.orchestrator]\nlogical_model = 'sol'\n",
    )
    .expect("config");

    config::save_sandbox(&path, config::SandboxConfig::default()).expect("save");

    let text = std::fs::read_to_string(&path).expect("text");
    assert!(text.contains("version = 3"), "{text}");
    assert!(text.contains("# keep panel"), "{text}");
    assert!(!text.contains("logical_model"), "{text}");
    assert!(!text.contains("reasoning_effort = \"xhigh\"\n"), "{text}");
    let config = Config::load_strict(&options(tmp.path())).expect("load");
    assert_eq!(
        config.agents.orchestrator.models,
        [ChainEntryConfig::with_effort("sol", "xhigh")]
    );
    assert!(!config.routing.routes["sol"][0].auto);
}

// Given: 2 要素のチェーンを持つ agents
// When: 保存して読み直す
// Then: 要素の順序と推論強度が往復する
#[test]
fn agent_bindings_round_trip_multi_entry_chain() {
    let tmp = tempfile::tempdir().expect("temp");
    let path = tmp.path().join("config.toml");
    let mut agents = config::AgentsConfig::default();
    agents.roles.planner.models = vec![
        ChainEntryConfig::with_effort("kimi", "high"),
        ChainEntryConfig::route("claude"),
    ];

    config::save_agent_bindings(&path, &agents).expect("save");

    let loaded = Config::load(&options(tmp.path())).expect("load");
    assert_eq!(loaded.agents, agents);
    let text = std::fs::read_to_string(&path).expect("text");
    assert!(
        text.contains(
            r#"models = [{ route = "kimi", reasoning_effort = "high" }, { route = "claude" }]"#
        ),
        "{text}"
    );
}
