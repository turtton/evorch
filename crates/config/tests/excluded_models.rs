//! 除外モデルの後方互換性と保存契約。

use config::{
    Config, LoadOptions, OpenAiCompatibleProviderInput, ProviderProfileConfig,
    save_openai_compatible_provider,
};

#[test]
fn exclusions_default_to_empty_when_field_is_omitted() {
    // Given: 既存形式のプロファイル
    let text = "type = 'openai-compatible'\nmodels = ['manual']\n";
    // When: ミラー経由で解析する
    let profile: ProviderProfileConfig = toml::from_str(text).unwrap();
    assert!(profile.excluded_models.is_empty());
    // Then: 公開シリアライズでも既定の空配列になる
    let value = toml::Value::try_from(profile).unwrap();
    assert_eq!(
        value.get("excluded_models"),
        Some(&toml::Value::Array(vec![]))
    );
}

#[test]
fn exclusions_round_trip_when_models_are_not_in_manual_list() {
    // Given: 手動モデル一覧に含まれない除外 ID
    let text = "type = 'openai-compatible'\nmodels = ['manual']\nexcluded_models = ['m1', 'm2']\n";
    // When: 解析して公開形式に戻す
    let profile: ProviderProfileConfig = toml::from_str(text).unwrap();
    assert_eq!(profile.excluded_models, ["m1", "m2"]);
    let value = toml::Value::try_from(profile).unwrap();
    // Then: 除外 ID と順序が維持される
    assert_eq!(
        value["excluded_models"],
        toml::Value::Array(vec!["m1".into(), "m2".into()])
    );
}

#[test]
fn unknown_field_is_rejected_when_exclusions_key_is_misspelled() {
    // Given: 除外設定のキーの typo
    let text = "excluded_model = ['m1']\n";
    // When: strict なミラーで解析する
    let error = toml::from_str::<ProviderProfileConfig>(text).unwrap_err();
    // Then: 未知フィールドとして拒否される
    assert!(error.to_string().contains("unknown field `excluded_model`"));
}

#[test]
fn strict_load_accepts_exclusions_when_present_in_project_config() {
    // Given: 実際のプロジェクト設定ファイル
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("evorch.toml"),
        "version = 2\n[providers.local]\nexcluded_models = ['m1', 'm2']\n",
    )
    .unwrap();
    // When: マージと strict validation を含むロードを実行する
    let config = Config::load(&LoadOptions {
        project_dir: Some(tmp.path().to_path_buf()),
        user_config_dir: Some(tmp.path().join("empty-user")),
        read_env: false,
        ..LoadOptions::default()
    })
    .unwrap();
    // Then: 除外 ID が保存されている
    let value = toml::Value::try_from(&config.providers["local"]).unwrap();
    assert_eq!(
        value["excluded_models"],
        toml::Value::Array(vec!["m1".into(), "m2".into()])
    );
}

#[test]
fn schema_exposes_exclusions_as_string_array() {
    // Given: 公開プロファイル型
    // When: JSON Schema を生成する
    let schema = schemars::schema_for!(ProviderProfileConfig);
    // Then: 任意の文字列配列フィールドとして公開される
    let value = serde_json::to_value(schema).unwrap();
    let field = &value["properties"]["excluded_models"];
    assert_eq!(field["type"], "array");
    assert_eq!(field["items"]["type"], "string");
    assert_eq!(field["default"], serde_json::json!([]));
}

#[test]
fn save_round_trips_exclusions_when_input_needs_normalization() {
    // Given: 手動一覧の外の ID と空白・空要素・重複
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    let input = OpenAiCompatibleProviderInput {
        name: "local".into(),
        base_url: "https://example.com/v1".into(),
        credential: config::ProviderCredentialInput::Env {
            var: "LOCAL_KEY".into(),
        },
        models: vec![config::types::provider::ModelEntryConfig::enabled("manual")],
        excluded_models: vec![" m1 ".into(), "".into(), "m2".into(), "m1".into()],
        default_model: "manual".into(),
    };
    // When: 公開 API でファイル保存する
    save_openai_compatible_provider(&path, &input).unwrap();
    // Then: 実ロード経路でも正規化された除外 ID が維持される
    let config = Config::load(&LoadOptions {
        project_dir: Some(tmp.path().to_path_buf()),
        user_config_dir: Some(tmp.path().join("empty-user")),
        read_env: false,
        ..LoadOptions::default()
    })
    .unwrap();
    assert_eq!(config.providers["local"].excluded_models, ["m1", "m2"]);
}

#[test]
fn save_omits_exclusions_when_normalized_input_is_empty() {
    for excluded_models in [vec![], vec![" ".into(), "".into()]] {
        // Given: 空、または正規化後に空になる除外一覧
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evorch.toml");
        let input = OpenAiCompatibleProviderInput {
            name: "local".into(),
            base_url: "https://example.com/v1".into(),
            credential: config::ProviderCredentialInput::Env {
                var: "LOCAL_KEY".into(),
            },
            models: vec![config::types::provider::ModelEntryConfig::enabled("manual")],
            excluded_models,
            default_model: "manual".into(),
        };
        // When: 公開 API で保存する
        save_openai_compatible_provider(&path, &input).unwrap();
        // Then: 空配列のキーを出力せず、再解析時には空一覧になる
        let text = std::fs::read_to_string(path).unwrap();
        let value: toml::Value = toml::from_str(&text).unwrap();
        assert!(value["providers"]["local"].get("excluded_models").is_none());
        let config: Config = value.try_into().unwrap();
        assert!(config.providers["local"].excluded_models.is_empty());
    }
}
