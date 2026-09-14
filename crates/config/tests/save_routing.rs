use config::{Config, LoadOptions, RouteCandidateConfig, RoutingConfig};

fn load(root: &std::path::Path) -> Config {
    Config::load(&LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("保存結果")
}

#[test]
fn roundtrip_preserves_routes_order_and_optional_override() {
    // Given: 特殊文字を含む論理名と順序付き候補。
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("evorch.toml");
    let routing = RoutingConfig {
        routes: [
            (
                "worker.v2/日本語".into(),
                vec![
                    RouteCandidateConfig {
                        profile: "second".into(),
                        model: Some("custom-model".into()),
                    },
                    RouteCandidateConfig {
                        profile: "first".into(),
                        model: None,
                    },
                ],
            ),
            (
                "explorer".into(),
                vec![RouteCandidateConfig {
                    profile: "first".into(),
                    model: None,
                }],
            ),
        ]
        .into(),
    };
    // When: routing を保存する。
    config::save_routing(&path, &routing).expect("save");
    // Then: 全候補の順序と省略を維持する。
    assert_eq!(load(temp.path()).routing, routing);
    let text = std::fs::read_to_string(path).expect("text");
    assert_eq!(text.matches("model =").count(), 1);
    assert_eq!(text.matches("[[routing.routes.").count(), 3);
}

#[test]
fn empty_routing_replaces_old_routes_and_preserves_other_sections() {
    // Given: コメント付き他セクションと既存ルート。
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("evorch.toml");
    let unrelated = "# keep header\nversion = 2 # keep version\n\n[agents.worker]\n# keep binding\nlogical_model = 'worker'\n\n[providers.local]\ntype = 'openai-compatible'\nbase_url = 'https://example.invalid/v1'\napi_key_env = 'KEY'\nmodels = ['base']\ndefault_model = 'base' # keep default\n";
    std::fs::write(
        &path,
        format!("{unrelated}\n[routing.routes]\nworker = [{{ profile = 'local' }}]\n"),
    )
    .expect("fixture");
    let before = load(temp.path());
    // When: 全ルートを削除して保存する。
    config::save_routing(&path, &RoutingConfig::default()).expect("save");
    // Then: routing だけ空になり他の表現は変わらない。
    let after = load(temp.path());
    assert!(after.routing.routes.is_empty());
    assert_eq!(after.providers, before.providers);
    assert_eq!(after.agents, before.agents);
    assert!(
        std::fs::read_to_string(path)
            .expect("text")
            .starts_with(unrelated)
    );
}
