use config::{Config, RouteCandidateConfig};
use gui::model::routing_settings::RoutingSettingsModel;

fn fixture() -> Config {
    let mut config = Config::default();
    config.providers.insert("local".into(), Default::default());
    config.routing.routes.insert(
        "worker".into(),
        vec![
            RouteCandidateConfig {
                profile: "local".into(),
                model: Some("custom".into()),
            },
            RouteCandidateConfig {
                profile: "local".into(),
                model: None,
            },
        ],
    );
    config.routing.routes.insert(
        "other".into(),
        vec![RouteCandidateConfig {
            profile: "local".into(),
            model: Some("external".into()),
        }],
    );
    config
}

#[test]
fn seed_preserves_all_routes_and_candidates() {
    // Given: 複数ルートと任意モデル。
    let config = fixture();
    // When: 設定を編集モデルへ取り込む。
    let model = RoutingSettingsModel::seed_from_config(&config);
    // Then: 全データと候補順を維持する。
    assert_eq!(model.routes, config.routing.routes);
    assert_eq!(model.profile_names, ["local"]);
    assert_eq!(model.validated_routing().expect("valid"), config.routing);
}

#[test]
fn validation_rejects_invalid_routes() {
    // Given: 空論理名、候補なし、未知または空プロファイル。
    for (name, candidates) in [
        (
            " ",
            vec![RouteCandidateConfig {
                profile: "local".into(),
                model: None,
            }],
        ),
        ("worker", vec![]),
        (
            "worker",
            vec![RouteCandidateConfig {
                profile: "unknown".into(),
                model: None,
            }],
        ),
        ("worker", vec![RouteCandidateConfig::default()]),
    ] {
        let mut model = RoutingSettingsModel::seed_from_config(&fixture());
        model.routes = [(name.into(), candidates)].into();
        // When: 保存用設定を検証する。
        let result = model.validated_routing();
        // Then: 不正入力は拒否する。
        assert!(result.is_err(), "{name}");
    }
}

#[test]
fn reorder_and_blank_override_are_preserved_for_save() {
    // Given: 優先度を入れ替え空白 override を入力する。
    let mut model = RoutingSettingsModel::seed_from_config(&fixture());
    let candidates = model.routes.get_mut("worker").expect("worker");
    candidates.swap(0, 1);
    candidates[0].model = Some("  ".into());
    // When: 保存用設定へ変換する。
    let routing = model.validated_routing().expect("valid");
    // Then: 空白は None、自由入力モデルはそのまま維持する。
    assert_eq!(routing.routes["worker"][0].model, None);
    assert_eq!(routing.routes["worker"][1].model.as_deref(), Some("custom"));
}

#[test]
fn duplicate_add_and_rename_never_overwrite_candidates() {
    // Given: 既存の異なるルート。
    let mut model = RoutingSettingsModel::seed_from_config(&fixture());
    let before = model.routes.clone();
    // When: 衝突する名前を指定する。
    assert!(model.add_route("worker").is_err());
    assert!(model.rename_route("other", "worker").is_err());
    assert!(model.add_route("  ").is_err());
    // Then: 既存データを上書きしない。
    assert_eq!(model.routes, before);
}

#[test]
fn draft_rename_preserves_candidates_and_rejects_collisions() {
    // Given: フォーカス保持用バッファで論理名を編集する。
    let mut model = RoutingSettingsModel::seed_from_config(&fixture());
    model
        .route_name_edits
        .insert("worker".into(), "renamed".into());
    // When: 編集済みの保存内容を構築する。
    let saved = model.validated_routing().expect("valid rename");
    // Then: 元の候補順を保持して新しい名前へ移す。
    assert_eq!(saved.routes["renamed"], fixture().routing.routes["worker"]);
    assert!(!saved.routes.contains_key("worker"));
}

#[test]
fn draft_rename_rejects_blank_and_duplicate_names() {
    // Given: 空白または他ルートと衝突する編集中の名前。
    for name in [" ", "other"] {
        let mut model = RoutingSettingsModel::seed_from_config(&fixture());
        model.route_name_edits.insert("worker".into(), name.into());
        // When: 保存内容を構築する。
        let result = model.validated_routing();
        // Then: 上書き保存を拒否する。
        assert!(result.is_err());
    }
}
