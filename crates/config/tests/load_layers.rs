//! レイヤード構成読み込み ([`config::Config::load`]) の統合テスト。
//!
//! すべてのテストは tempfile を用い、実環境の `~/.config` や実際の環境変数には
//! 一切依存しない (`user_config_dir` と `env` を常に注入する)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::{CURRENT_VERSION, Config, ConfigError, LoadOptions};

/// テスト用に親ディレクトリを作成してからファイルを書き込む。
fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("親ディレクトリを作成できる");
    }
    std::fs::write(path, content).expect("テストファイルを書き込める");
}

/// 一時ディレクトリ配下の空の (まだ存在しない) ユーザ設定ディレクトリを返す。
fn empty_user_dir(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("user-empty")
}

/// 注入用の環境変数マップを作成する。
fn env_vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

// Given: どのレイヤーにもファイルが存在しない / When: 読み込みオプションを与えて読み込む
// Then: 組み込み既定値と完全に等しい設定が得られる
#[test]
fn builtin_defaults_used_when_no_files() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(empty_user_dir(&tmp)),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("全レイヤー不在でも読み込みできる");

    assert_eq!(config, Config::default());
    assert_eq!(config.version, CURRENT_VERSION);
}

// Given: ユーザ層のメインファイルと 2 つのドロップイン / When: 読み込む
// Then: 辞書順で後のドロップインが優先され、メインファイルのみのキーは維持される
#[test]
fn dropins_override_main_file_lexicographic() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let user = tmp.path().join("user");
    write_file(
        &user.join("config.toml"),
        "[metrics]\nenabled = false\nretention_days = 7\n",
    );
    write_file(
        &user.join("config.d/00-base.toml"),
        "[metrics]\nenabled = true\n",
    );
    write_file(
        &user.join("config.d/10-extra.toml"),
        "[metrics]\nenabled = false\n",
    );

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(user),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("読み込みできる");

    assert!(
        !config.metrics.enabled,
        "辞書順で後のドロップインが優先される"
    );
    assert_eq!(
        config.metrics.retention_days, 7,
        "メインファイルのみのキーはドロップイン後も維持される"
    );
}

// Given: ユーザ層の設定・プロファイルと、他のキーも書いたプロジェクト層 / When: 読み込む
// Then: プロジェクト層は role_profile だけを決め (ドロップインが優先)、他のキーは無視される
#[test]
fn project_layer_only_selects_role_profile() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let user = tmp.path().join("user");
    let project = tmp.path().join("project");
    write_file(
        &user.join("config.toml"),
        "[permissions]\npreset = \"permissive\"\n\
         [agents.worker]\nlogical_model = \"default-model\"\n\
         [role_profiles.fast.agents.worker]\nlogical_model = \"fast-model\"\n\
         [role_profiles.fast.routing.routes]\nfast-model = [{ profile = \"local\" }]\n",
    );
    write_file(
        &project.join(".evorch/config.toml"),
        "role_profile = \"slow\"\n[permissions]\npreset = \"strict\"\n\
         [agents.worker]\nlogical_model = \"project-model\"\n",
    );
    write_file(
        &project.join(".evorch/config.d/20-select.toml"),
        "role_profile = \"fast\"\n[panel]\nlayout = \"compact\"\n",
    );
    let options = LoadOptions {
        project_dir: Some(project.clone()),
        user_config_dir: Some(user),
        read_env: false,
        ..LoadOptions::default()
    };

    for config in [
        Config::load(&options).expect("読み込みできる"),
        Config::load_strict(&options).expect("厳格に読み込みできる"),
    ] {
        assert_eq!(config.permissions.preset, "permissive");
        assert_eq!(config.panel, config::PanelConfig::default());
        assert_eq!(config.role_profile.as_deref(), Some("fast"));
        assert_eq!(config.active_role_profile(), "fast");
        assert_eq!(
            config.agents.worker.logical_model.as_deref(),
            Some("fast-model"),
            "選択したプロファイルが実効の agents になる"
        );
        assert!(config.routing.routes.contains_key("fast-model"));
    }
    let unresolved = Config::load_unresolved(&options).expect("選択を反映せず読み込める");
    assert_eq!(
        unresolved.agents.worker.logical_model.as_deref(),
        Some("default-model")
    );
    assert_eq!(unresolved.role_profile_names(), ["default", "fast"]);
    assert_eq!(
        config::project_role_profile(&project).expect("プロジェクトの選択"),
        Some("fast".into())
    );
}

// Given: 存在しないプロファイルを選ぶプロジェクト層 / When: 読み込む
// Then: 読み込みは失敗せず、トップレベル (default) の割り当てを使う
#[test]
fn unknown_role_profile_falls_back_to_default() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let user = tmp.path().join("user");
    let project = tmp.path().join("project");
    write_file(
        &user.join("config.toml"),
        "[agents.worker]\nlogical_model = \"default-model\"\n",
    );
    write_file(
        &project.join(".evorch/config.toml"),
        "role_profile = \"missing\"\n",
    );

    let config = Config::load_strict(&LoadOptions {
        project_dir: Some(project),
        user_config_dir: Some(user),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("未知のプロファイルでも読み込める");

    assert_eq!(config.active_role_profile(), "default");
    assert_eq!(
        config.agents.worker.logical_model.as_deref(),
        Some("default-model")
    );
}

// Given: 設定ファイルと注入環境変数 / When: 環境変数レイヤーを有効にして読み込む
// Then: 環境変数が設定ファイルに優先する。read_env = false ならスキップされる
// (既定のテーブル構造に合わせ、ログディレクトリ型フィールド log_dir を使用する)
#[test]
fn env_overrides_config_file() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = tmp.path().join("project");
    write_file(
        &project.join(".evorch/config.toml"),
        "[diagnostics]\nlog_level = \"warn\"\nlog_dir = \"/tmp/from-file\"\n",
    );

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(project.join(".evorch")),
        read_env: true,
        env: Some(env_vars(&[(
            "EVORCH_DIAGNOSTICS__LOG_DIR",
            "/tmp/from-env",
        )])),
        ..LoadOptions::default()
    })
    .expect("読み込みできる");
    assert_eq!(
        config.diagnostics.log_dir.as_deref(),
        Some("/tmp/from-env"),
        "環境変数がプロジェクトファイルに優先する"
    );
    assert_eq!(
        config.diagnostics.log_level, "warn",
        "環境変数で上書きしていないキーは維持される"
    );

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(project.join(".evorch")),
        read_env: false,
        env: Some(env_vars(&[(
            "EVORCH_DIAGNOSTICS__LOG_DIR",
            "/tmp/from-env",
        )])),
        ..LoadOptions::default()
    })
    .expect("読み込みできる");
    assert_eq!(
        config.diagnostics.log_dir.as_deref(),
        Some("/tmp/from-file"),
        "read_env = false なら環境変数レイヤーはスキップされる"
    );
}

// Given: CLI 上書きと環境変数の両方で同じキー / When: 読み込む
// Then: CLI 上書きが環境変数層より優先する
#[test]
fn cli_overrides_env() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = tmp.path().join("project");
    write_file(
        &project.join(".evorch/config.toml"),
        "[diagnostics]\nlog_dir = \"/tmp/from-file\"\n",
    );
    let cli_overrides = toml::from_str(
        r#"
        [diagnostics]
        log_dir = "/tmp/from-cli"
        "#,
    )
    .expect("CLI 上書き TOML を解析できる");

    let config = Config::load(&LoadOptions {
        project_dir: None,
        user_config_dir: Some(project.join(".evorch")),
        cli_overrides: Some(cli_overrides),
        file_overrides: BTreeMap::new(),
        read_env: true,
        env: Some(env_vars(&[(
            "EVORCH_DIAGNOSTICS__LOG_DIR",
            "/tmp/from-env",
        )])),
    })
    .expect("読み込みできる");

    assert_eq!(
        config.diagnostics.log_dir.as_deref(),
        Some("/tmp/from-cli"),
        "CLI 上書きが環境変数層より優先する"
    );
}

// Given: TOML リテラルとして妥当な値と妥当でない値の環境変数 / When: 読み込む
// Then: 妥当な値はリテラルとして解釈され、妥当でない値は生の文字列として扱われる
#[test]
fn env_value_parsed_as_toml_literal_with_string_fallback() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let env = env_vars(&[
        ("EVORCH_METRICS__ENABLED", "false"),
        ("EVORCH_PERMISSIONS__PRESET", "hello"),
    ]);

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(empty_user_dir(&tmp)),
        read_env: true,
        env: Some(env),
        ..LoadOptions::default()
    })
    .expect("読み込みできる");

    assert!(
        !config.metrics.enabled,
        "\"false\" は boolean リテラルとして解釈される (既定値は true)"
    );
    assert_eq!(
        config.permissions.preset, "hello",
        "TOML リテラルでない値は生の文字列として扱われる (既定値は \"default\")"
    );
}

// Given: 壊れた TOML のプロジェクトファイル / When: 読み込む
// Then: 問題のファイルパスを含む Parse エラーになる
#[test]
fn parse_error_reports_offending_path() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = tmp.path().join("project");
    let broken = project.join(".evorch/config.toml");
    write_file(&broken, "= broken [metrics\n");

    let error = Config::load(&LoadOptions {
        user_config_dir: Some(project.join(".evorch")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect_err("壊れた TOML はエラーになる");

    match error {
        ConfigError::Parse { path, .. } => {
            assert_eq!(path, broken, "エラーが問題のファイルパスを含む");
        }
        other => panic!("Parse エラーを期待したが {other:?} を得た"),
    }
}

// Given: version が現行より大きい設定ファイル / When: 読み込む
// Then: UnsupportedVersion エラーになる
#[test]
fn future_version_is_rejected() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = tmp.path().join("project");
    write_file(&project.join(".evorch/config.toml"), "version = 999\n");

    let error = Config::load(&LoadOptions {
        user_config_dir: Some(project.join(".evorch")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect_err("未来のバージョンは拒否される");

    match error {
        ConfigError::UnsupportedVersion { found, current } => {
            assert_eq!(found, 999);
            assert_eq!(current, CURRENT_VERSION);
        }
        other => panic!("UnsupportedVersion エラーを期待したが {other:?} を得た"),
    }
}

// Given: metrics セクションを持たないバージョン 1 のプロジェクト設定 / When: 読み込む
// Then: バージョン 2 へ移行され、metrics は既定値で補完される
#[test]
fn migrate_v1_file_gains_metrics_defaults() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = tmp.path().join("project");
    write_file(&project.join(".evorch/config.toml"), "version = 1\n");

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(project.join(".evorch")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("バージョン 1 の設定を移行して読み込める");

    assert_eq!(config.version, CURRENT_VERSION);
    assert_eq!(config.metrics, config::MetricsConfig::default());
}

// Given: enabled のみを上書きしたバージョン 1 の metrics 設定 / When: 読み込む
// Then: ユーザ値を維持し、未指定の retention_days は既定値で補完される
#[test]
fn v1_file_with_partial_metrics_keeps_user_values() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = tmp.path().join("project");
    write_file(
        &project.join(".evorch/config.toml"),
        "version = 1\n\n[metrics]\nenabled = false\n",
    );

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(project.join(".evorch")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("部分的な metrics を持つバージョン 1 の設定を移行して読み込める");

    assert!(!config.metrics.enabled);
    assert_eq!(
        config.metrics.retention_days,
        config::MetricsConfig::default().retention_days
    );
}

// Given: metrics を上書きするバージョン 1 のメインとバージョン 2 のドロップイン / When: 読み込む
// Then: 各ファイルがマージ前に移行され、ドロップインがメインへ優先する
#[test]
fn mixed_version_layers_each_migrated_before_merge() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let user = tmp.path().join("user");
    write_file(
        &user.join("config.toml"),
        "version = 1\n\n[metrics]\nenabled = false\n",
    );
    write_file(
        &user.join("config.d/10-current.toml"),
        "version = 2\n\n[metrics]\nretention_days = 7\n",
    );

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(user),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("混在バージョンのレイヤーを読み込める");

    assert_eq!(config.version, CURRENT_VERSION);
    assert!(!config.metrics.enabled);
    assert_eq!(config.metrics.retention_days, 7);
}

// Given: version キーを持たず metrics を上書きする設定 / When: 読み込む
// Then: 現行バージョンとして扱われ、metrics の上書き値を維持する
#[test]
fn missing_version_treated_as_current() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = tmp.path().join("project");
    write_file(
        &project.join(".evorch/config.toml"),
        "[metrics]\nenabled = false\n",
    );

    let config = Config::load(&LoadOptions {
        user_config_dir: Some(project.join(".evorch")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("version がない設定を現行として読み込める");

    assert!(!config.metrics.enabled);
}

// Given: リーフパスが衝突する 2 つの環境変数 / When: 読み込む
// Then: InvalidEnvValue エラーになり、処理中の変数名が報告される
#[test]
fn env_leaf_path_collision_rejected() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let env = env_vars(&[
        ("EVORCH_PANEL", "compact"),
        ("EVORCH_PANEL__LAYOUT", "wide"),
    ]);

    let error = Config::load(&LoadOptions {
        user_config_dir: Some(empty_user_dir(&tmp)),
        read_env: true,
        env: Some(env),
        ..LoadOptions::default()
    })
    .expect_err("リーフパスの衝突はエラーになる");

    match error {
        ConfigError::InvalidEnvValue { key, value } => {
            assert_eq!(key, "EVORCH_PANEL__LAYOUT");
            assert_eq!(value, "wide");
        }
        other => panic!("InvalidEnvValue エラーを期待したが {other:?} を得た"),
    }
}
