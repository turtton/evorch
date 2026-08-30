//! 厳格な設定フィールド拒否の統合テスト。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::{Config, ConfigError, CredentialRefConfig, LoadOptions};

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("親ディレクトリを作成できる");
    }
    std::fs::write(path, content).expect("テストファイルを書き込める");
}

fn empty_user_dir(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("user-empty")
}

fn env_vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn load_project(tmp: &tempfile::TempDir, content: &str) -> Result<Config, ConfigError> {
    let project = tmp.path().join("project");
    write_file(&project.join("evorch.toml"), content);
    Config::load(&LoadOptions {
        project_dir: Some(project),
        user_config_dir: Some(empty_user_dir(tmp)),
        read_env: false,
        ..LoadOptions::default()
    })
}

fn assert_error_contains(result: Result<Config, ConfigError>, fragments: &[&str]) {
    let err = result.expect_err("不正な設定は拒否される");
    let msg = err.to_string();
    for fragment in fragments {
        assert!(msg.contains(fragment), "{fragment:?} not found in {msg:?}");
    }
}

// Given: providers.foo に秘密値そのものを含む設定 / When: 読み込む
// Then: パスと安全な参照方法を含むエラーとして拒否される
#[test]
fn plaintext_api_key_rejected_with_path_and_remediation() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");

    // Given: 平文 API キーを含むプロジェクト設定
    let result = load_project(&tmp, "[providers.foo]\napi_key = \"sk-test\"\n");

    // When: 設定を読み込む
    let err = result.expect_err("平文 API キーは拒否される");
    println!("REJECT-ERROR: {err}");

    // Then: パスと remediation がエラーに含まれる
    let msg = err.to_string();
    assert!(msg.contains("providers.foo.api_key"));
    assert!(msg.contains("keyring"));
    assert!(msg.contains("env"));
    assert!(msg.contains("credential"));
}

// Given: providers.foo に秘密値らしい別名を含む設定 / When: 読み込む
// Then: 各キーがパス付きエラーとして拒否される
#[test]
fn credential_like_aliases_rejected() {
    for key in ["api-key", "token", "secret", "password", "credential_value"] {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
        let content = format!("[providers.foo]\n{key} = \"x\"\n");

        // Given/When: 秘密値らしい別名を含む設定を読み込む
        let result = load_project(&tmp, &content);

        // Then: キーのパスと remediation がエラーに含まれる
        let path = format!("providers.foo.{key}");
        assert_error_contains(result, &[&path, "keyring", "var"]);
    }
}

// Given: ルートセクション名の typo / When: 読み込む
// Then: typo のパスと unknown field を含むエラーになる
#[test]
fn root_typo_rejected_with_path() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    assert_error_contains(
        load_project(&tmp, "[diagnotics]\nlog_level = \"debug\"\n"),
        &["diagnotics", "unknown field"],
    );
}

// Given: diagnostics 内のフィールド名 typo / When: 読み込む
// Then: typo の完全なパスを含むエラーになる
#[test]
fn nested_typo_rejected_with_path() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    assert_error_contains(
        load_project(&tmp, "[diagnostics]\nlog_lvl = \"debug\"\n"),
        &["diagnostics.log_lvl"],
    );
}

// Given: provider の未知の非秘密フィールド / When: 読み込む
// Then: unknown field エラーとして拒否される
#[test]
fn provider_unknown_noncredential_key_rejected() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    assert_error_contains(
        load_project(&tmp, "[providers.foo]\ntimeout = 5\n"),
        &["providers.foo.timeout", "unknown field"],
    );
}

// Given: credential variant に余分なキーを含む設定 / When: 読み込む
// Then: variant 内の余分なキーがパス付きで拒否される
#[test]
fn credential_variant_extra_keys_rejected_via_load() {
    let keyring = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    assert_error_contains(
        load_project(
            &keyring,
            "[providers.foo]\ncredential = { type = \"keyring\", service = \"s\", account = \"a\", token = \"x\" }\n",
        ),
        &["providers.foo.credential.token"],
    );

    let env = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    assert_error_contains(
        load_project(
            &env,
            "[providers.foo]\ncredential = { type = \"env\", var = \"V\", api_key = \"y\" }\n",
        ),
        &["providers.foo.credential.api_key"],
    );
}

// Given: 完全な keyring プロファイル / When: 読み込む
// Then: keyring credential として正常に読み込める
#[test]
fn valid_keyring_profile_loads() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let config = load_project(
        &tmp,
        r#"version = 2

[providers.anthropic-main]
provider_type = "anthropic"
api_protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = { type = "keyring", service = "evorch", account = "anthropic-main" }
models = ["claude-sonnet-4-5"]
default_model = "claude-sonnet-4-5"
"#,
    )
    .expect("有効な keyring プロファイルを読み込める");

    let profile = config
        .providers
        .get("anthropic-main")
        .expect("プロファイルが存在する");
    match &profile.credential {
        CredentialRefConfig::Keyring { service, account } => {
            assert_eq!(service, "evorch");
            assert_eq!(account, "anthropic-main");
        }
        CredentialRefConfig::Env { .. } => panic!("keyring credential を期待した"),
    }
    println!("PARSED-CONFIG: {config:?}");
}

// Given: 完全な env プロファイル / When: 読み込む
// Then: env credential として正常に読み込める
#[test]
fn valid_env_profile_loads() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let config = load_project(
        &tmp,
        "[providers.openrouter-main]\nprovider_type = \"openrouter\"\napi_protocol = \"openai-completions\"\ncredential = { type = \"env\", var = \"OPENROUTER_API_KEY\" }\n",
    )
    .expect("有効な env プロファイルを読み込める");

    let profile = config
        .providers
        .get("openrouter-main")
        .expect("プロファイルが存在する");
    match &profile.credential {
        CredentialRefConfig::Keyring { .. } => panic!("env credential を期待した"),
        CredentialRefConfig::Env { var } => assert_eq!(var, "OPENROUTER_API_KEY"),
    }
}

// Given: ドロップインに平文秘密値を含む設定 / When: 読み込む
// Then: パスと安全な参照方法を含むエラーになる
#[test]
fn dropin_source_secret_rejected() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = tmp.path().join("project");
    write_file(
        &project.join("config.d/50-secret.toml"),
        "[providers.foo]\napi_key = \"x\"\n",
    );
    assert_error_contains(
        Config::load(&LoadOptions {
            project_dir: Some(project),
            user_config_dir: Some(empty_user_dir(&tmp)),
            read_env: false,
            ..LoadOptions::default()
        }),
        &["providers.foo.api_key", "keyring", "env", "credential"],
    );
}

// Given: 環境変数層に平文秘密値を含む設定 / When: 読み込む
// Then: providers.foo.api_key と keyring を含むエラーになる
#[test]
fn env_layer_secret_rejected() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    assert_error_contains(
        Config::load(&LoadOptions {
            user_config_dir: Some(empty_user_dir(&tmp)),
            read_env: true,
            env: Some(env_vars(&[("EVORCH_PROVIDERS__FOO__API_KEY", "x")])),
            ..LoadOptions::default()
        }),
        &["providers.foo.api_key", "keyring"],
    );
}

// Given: CLI 上書き層に平文秘密値を含む設定 / When: 読み込む
// Then: パスと安全な参照方法を含むエラーになる
#[test]
fn cli_override_secret_rejected() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let cli_overrides: toml::Value =
        toml::from_str("[providers.foo]\napi_key = \"x\"\n").expect("CLI 上書き TOML を解析できる");
    assert_error_contains(
        Config::load(&LoadOptions {
            user_config_dir: Some(empty_user_dir(&tmp)),
            cli_overrides: Some(cli_overrides),
            read_env: false,
            ..LoadOptions::default()
        }),
        &["providers.foo.api_key", "keyring", "env", "credential"],
    );
}

// Given: 移行対象の v1 ファイルに平文秘密値を含む設定 / When: 読み込む
// Then: 移行後も平文秘密値が拒否される
#[test]
fn v1_file_secret_rejected_after_migration() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    assert_error_contains(
        load_project(&tmp, "version = 1\n\n[providers.foo]\napi_key = \"x\"\n"),
        &["providers.foo.api_key", "keyring", "env", "credential"],
    );
}

// Given: パネルに任意のキーバインドを含む設定 / When: 読み込む
// Then: 任意のキーが許容される
#[test]
fn panel_keybinds_arbitrary_keys_allowed() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let config = load_project(&tmp, "[panel.keybinds]\nmy_custom_action = \"x\"\n")
        .expect("任意のキーバインドを読み込める");
    assert_eq!(
        config.panel.keybinds.get("my_custom_action"),
        Some(&"x".to_string())
    );
}

// Given: routing の候補に未知のキーを含む設定 / When: 読み込む
// Then: 候補のインデックス付きパスを含むエラーになる
#[test]
fn route_candidate_unknown_key_rejected() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    assert_error_contains(
        load_project(
            &tmp,
            "[[routing.routes.fast]]\nprofile = \"p\"\nweight = 3\n",
        ),
        &["routing.routes.fast[0].weight"],
    );
}
