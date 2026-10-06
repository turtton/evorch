//! ロール構成プロファイルの検証・保存の統合テスト。

use std::path::Path;

use config::{
    Config, ConfigError, LoadOptions, RoleProfileConfig, RouteCandidateConfig, RoutingConfig,
    delete_role_profile, project_role_profile, save_project_role_profile, save_role_profile,
    save_role_profile_bindings,
};

fn options(user: &Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(user.to_path_buf()),
        read_env: false,
        ..LoadOptions::default()
    }
}

fn load_user(text: &str, strict: bool) -> Result<Config, ConfigError> {
    let tmp = tempfile::tempdir().expect("temp");
    std::fs::write(tmp.path().join("config.toml"), text).expect("config");
    if strict {
        Config::load_strict(&options(tmp.path()))
    } else {
        Config::load_unresolved(&options(tmp.path()))
    }
}

fn invalid_path(result: Result<Config, ConfigError>) -> String {
    match result {
        Err(ConfigError::InvalidField { path, .. }) => path,
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

fn routing(route: &str, profile: &str) -> RoutingConfig {
    RoutingConfig {
        routes: [(
            route.to_owned(),
            vec![RouteCandidateConfig {
                profile: profile.to_owned(),
                ..Default::default()
            }],
        )]
        .into(),
    }
}

// Given: プロファイル内の不正な名前・予約名・未知キー・分類不可のカテゴリ
// When: 厳格に読み込む / Then: プロファイルを含む完全なパスで拒否される
#[test]
fn strict_load_validates_profiles_with_full_paths() {
    for (text, path) in [
        (
            "[role_profiles.default.agents.worker]\nlogical_model = 'x'\n",
            "role_profiles.default",
        ),
        (
            "[role_profiles.Fast.agents.worker]\nlogical_model = 'x'\n",
            "role_profiles.Fast",
        ),
        (
            "[role_profiles.fast]\nproviders = {}\n",
            "role_profiles.fast.providers",
        ),
        (
            "[role_profiles.fast.agents.worker]\nlogical_modle = 'x'\n",
            "role_profiles.fast.agents.worker.logical_modle",
        ),
        (
            "[role_profiles.fast.agents.explorer.categories.quick]\nlogical_model = 'x'\n",
            "role_profiles.fast.agents.explorer.categories",
        ),
        (
            "[[role_profiles.fast.routing.routes.main]]\nprofile = 'p'\nweight = 1\n",
            "role_profiles.fast.routing.routes.main[0].weight",
        ),
    ] {
        assert_eq!(invalid_path(load_user(text, true)), path, "{text}");
    }
}

// Given: プロファイル内に typo を含む設定 / When: 寛容に読み込む
// Then: typo だけが除かれ、正しいバインディングとカテゴリは残る
#[test]
fn tolerant_load_prunes_unknown_profile_fields_and_keeps_bindings() {
    let config = load_user(
        "[role_profiles.fast]\nextra = 1\n\
         [role_profiles.fast.agents.worker]\nlogical_model = 'fast'\nlogical_modle = 'typo'\n\
         [role_profiles.fast.agents.worker.categories.quick]\nlogical_model = 'quick'\n",
        false,
    )
    .expect("typo は無視される");
    let worker = &config.role_profiles["fast"].agents.worker;
    assert_eq!(worker.base.logical_model.as_deref(), Some("fast"));
    assert_eq!(
        worker.categories["quick"].logical_model.as_deref(),
        Some("quick")
    );
}

// Given: コメント付きのユーザ設定 / When: プロファイルを作成・更新・削除する
// Then: 対象テーブルだけが変わり、トップレベルとコメントは保持される
#[test]
fn profile_saves_round_trip_and_preserve_unrelated_content() {
    let tmp = tempfile::tempdir().expect("temp");
    let path = tmp.path().join("config.toml");
    std::fs::write(
        &path,
        "# keep me\n[agents.worker]\nlogical_model = 'default-model'\n",
    )
    .expect("config");
    let base = Config::load_unresolved(&options(tmp.path())).expect("load");
    let mut copied = base.role_profile_config(None).expect("default profile");
    copied.routing = routing("fast-model", "local");

    save_role_profile(&path, "fast", &copied).expect("create");
    let mut agents = copied.agents.clone();
    agents.worker.base.logical_model = Some("fast-model".into());
    save_role_profile_bindings(&path, Some("fast"), None, Some(&agents)).expect("update agents");

    let saved = Config::load_strict(&options(tmp.path())).expect("strict reload");
    assert_eq!(
        saved.role_profiles["fast"],
        RoleProfileConfig {
            agents: agents.clone(),
            routing: routing("fast-model", "local"),
        }
    );
    assert_eq!(
        saved.agents.worker.base.logical_model.as_deref(),
        Some("default-model")
    );
    assert!(
        std::fs::read_to_string(&path)
            .expect("text")
            .contains("# keep me\n")
    );

    save_role_profile_bindings(&path, None, Some(&routing("main", "local")), None)
        .expect("update default routing");
    let saved = Config::load_unresolved(&options(tmp.path())).expect("reload");
    assert_eq!(saved.routing, routing("main", "local"));

    delete_role_profile(&path, "fast").expect("delete");
    let saved = Config::load_unresolved(&options(tmp.path())).expect("reload");
    assert!(saved.role_profiles.is_empty());
    assert_eq!(saved.routing, routing("main", "local"));
}

// Given: 存在しない・予約済みのプロファイル / When: 保存する
// Then: ファイルを変えずに拒否される
#[test]
fn profile_saves_reject_missing_and_reserved_names() {
    let tmp = tempfile::tempdir().expect("temp");
    let path = tmp.path().join("config.toml");
    std::fs::write(&path, "# untouched\n").expect("config");
    assert!(save_role_profile_bindings(&path, Some("missing"), None, None).is_err());
    assert!(save_role_profile(&path, "default", &RoleProfileConfig::default()).is_err());
    assert!(save_role_profile(&path, "Bad Name", &RoleProfileConfig::default()).is_err());
    assert!(delete_role_profile(&path, "default").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# untouched\n");
}

// Given: プロジェクトディレクトリ / When: role_profile を設定・解除する
// Then: プロジェクト設定だけが更新され、default の選択はキーを残さない
#[test]
fn project_role_profile_selection_round_trips() {
    let tmp = tempfile::tempdir().expect("temp");
    let project = tmp.path().join("project");
    assert_eq!(project_role_profile(&project).expect("missing file"), None);

    save_project_role_profile(&project, Some("fast")).expect("select");
    assert_eq!(
        project_role_profile(&project).expect("selected"),
        Some("fast".into())
    );

    save_project_role_profile(&project, Some("default")).expect("clear");
    assert_eq!(project_role_profile(&project).expect("cleared"), None);
    let text = std::fs::read_to_string(config::project_main_config_path(&project)).unwrap();
    assert!(!text.contains("role_profile"), "{text}");
    assert!(save_project_role_profile(&project, Some("Bad Name")).is_err());
}
