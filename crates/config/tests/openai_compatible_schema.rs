//! openai-compatible sugar 形式 (type エイリアス + api_key_env) の
//! `Config::load` 経路での正規化を検証する統合テスト。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::{
    ApiProtocolConfig, Config, ConfigError, CredentialRefConfig, LoadOptions, ProviderTypeConfig,
};

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

// Given: TARGET TOML FORM の openai-compatible sugar 形式 / When: Config::load で読み込む
// Then: type エイリアスが OpenAiCompatible に、api_key_env が env credential に、
//       省略 api_protocol が OpenAiCompletions に正規化され、残りのフィールドは
//       記述どおりに読み込まれる
#[test]
fn openai_compatible_sugar_form_full_load() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let config = load_project(
        &tmp,
        r#"version = 2

[providers.local]
type = "openai-compatible"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "LOCAL_API_KEY"
models = ["local-model"]
default_model = "local-model"
"#,
    )
    .expect("sugar 形式のプロジェクト設定を読み込める");

    let profile = config
        .providers
        .get("local")
        .expect("プロファイルが存在する");
    assert_eq!(profile.provider_type, ProviderTypeConfig::OpenAiCompatible);
    assert_eq!(
        profile.credential,
        CredentialRefConfig::Env {
            var: "LOCAL_API_KEY".to_string()
        }
    );
    assert_eq!(profile.api_protocol, ApiProtocolConfig::OpenAiCompletions);
    assert_eq!(profile.base_url, "http://127.0.0.1:8080/v1");
    assert_eq!(profile.models, ["local-model"]);
    assert_eq!(profile.default_model, "local-model");
}

// Given: sugar 形式を環境変数層から上書きする設定 / When: Config::load で読み込む
// Then: 環境変数層経由の sugar キーも正規化される
#[test]
fn openai_compatible_sugar_form_via_env_layer() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let config = Config::load(&LoadOptions {
        user_config_dir: Some(empty_user_dir(&tmp)),
        read_env: true,
        env: Some(env_vars(&[
            ("EVORCH_PROVIDERS__LOCAL__TYPE", "openai-compatible"),
            (
                "EVORCH_PROVIDERS__LOCAL__BASE_URL",
                "http://127.0.0.1:8080/v1",
            ),
            ("EVORCH_PROVIDERS__LOCAL__API_KEY_ENV", "LOCAL_API_KEY"),
            ("EVORCH_PROVIDERS__LOCAL__MODELS", "[\"local-model\"]"),
            ("EVORCH_PROVIDERS__LOCAL__DEFAULT_MODEL", "local-model"),
        ])),
        ..LoadOptions::default()
    })
    .expect("環境変数層の sugar 形式を読み込める");

    let profile = config
        .providers
        .get("local")
        .expect("プロファイルが存在する");
    assert_eq!(profile.provider_type, ProviderTypeConfig::OpenAiCompatible);
    assert_eq!(profile.api_protocol, ApiProtocolConfig::OpenAiCompletions);
    match &profile.credential {
        CredentialRefConfig::Env { var } => assert_eq!(var, "LOCAL_API_KEY"),
        CredentialRefConfig::Keyring { .. } => panic!("env credential を期待した"),
    }
}
