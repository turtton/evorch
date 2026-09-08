//! プロバイダ書き戻しの fail-closed 契約。

use std::path::Path;

use config::{
    ApiProtocolConfig, Config, ConfigError, CredentialRefConfig, LoadOptions,
    OpenAiCompatibleProviderInput, ProviderTypeConfig, save_openai_compatible_provider,
    validate_openai_compatible_provider_input,
};

fn input() -> OpenAiCompatibleProviderInput {
    OpenAiCompatibleProviderInput {
        name: "local".into(),
        base_url: " http://localhost:11434/v1 ".into(),
        credential: config::ProviderCredentialInput::Env {
            var: "LOCAL_API_KEY".into(),
        },
        models: vec![
            " model-a ".into(),
            "".into(),
            "model-b".into(),
            "model-a".into(),
        ],
        default_model: "model-a".into(),
        excluded_models: vec![],
    }
}

#[test]
fn save_writes_keyring_credential_inline_table_and_omits_api_key_env() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let mut candidate = input();
    candidate.name = "openai-compat".into();
    candidate.credential = config::ProviderCredentialInput::Keyring {
        service: "evorch".into(),
        account: "openai-compat".into(),
    };
    // When
    save_openai_compatible_provider(&tmp.path().join("evorch.toml"), &candidate).unwrap();
    // Then
    let text = std::fs::read_to_string(tmp.path().join("evorch.toml")).unwrap();
    assert!(text.contains("credential = {"));
    assert!(!text.contains("api_key_env"));
    assert_eq!(
        load(tmp.path()).providers["openai-compat"].credential,
        CredentialRefConfig::Keyring {
            service: "evorch".into(),
            account: "openai-compat".into()
        }
    );
}

#[test]
fn save_replacing_env_profile_with_keyring_drops_api_key_env() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    let mut candidate = input();
    save_openai_compatible_provider(&path, &candidate).unwrap();
    candidate.credential = config::ProviderCredentialInput::Keyring {
        service: "evorch".into(),
        account: "local".into(),
    };
    // When
    save_openai_compatible_provider(&path, &candidate).unwrap();
    // Then
    assert!(
        !std::fs::read_to_string(path)
            .unwrap()
            .contains("api_key_env")
    );
}

#[test]
fn validation_rejects_empty_keyring_account() {
    // Given
    let mut candidate = input();
    candidate.credential = config::ProviderCredentialInput::Keyring {
        service: "evorch".into(),
        account: String::new(),
    };
    // When
    let result = validate_openai_compatible_provider_input(&candidate);
    // Then
    assert_field(result, "providers.local.credential.account");
}

fn load(dir: &Path) -> Config {
    Config::load(&LoadOptions {
        project_dir: Some(dir.to_path_buf()),
        user_config_dir: Some(dir.join("user-empty")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("保存結果を読み込める")
}

fn assert_field(result: Result<(), ConfigError>, expected: &str) {
    assert!(
        matches!(&result, Err(ConfigError::InvalidField { path, .. }) if path == expected),
        "expected InvalidField at {expected}, got {result:?}"
    );
}

#[test]
fn save_into_missing_file_creates_v2_sugar_entry() {
    // Given: 設定ファイルがまだない
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    // When: sugar 形式で保存する
    save_openai_compatible_provider(&path, &input()).expect("missing file save succeeds");
    // Then: 正規化した sugar が実際のロード経路で正規形になる
    let text = std::fs::read_to_string(path).unwrap();
    for fragment in [
        "version = 2",
        "[providers.local]",
        "type = \"openai-compatible\"",
        "api_key_env = \"LOCAL_API_KEY\"",
        "models = [\"model-a\", \"model-b\"]",
        "default_model = \"model-a\"",
    ] {
        assert!(text.contains(fragment), "missing {fragment}: {text}");
    }
    let config = load(tmp.path());
    let profile = &config.providers["local"];
    assert_eq!(profile.provider_type, ProviderTypeConfig::OpenAiCompatible);
    assert_eq!(
        profile.credential,
        CredentialRefConfig::Env {
            var: "LOCAL_API_KEY".into()
        }
    );
    assert_eq!(profile.api_protocol, ApiProtocolConfig::OpenAiCompletions);
    assert_eq!(profile.base_url, "http://localhost:11434/v1");
    assert_eq!(profile.models, ["model-a", "model-b"]);
}

#[test]
fn save_preserves_unrelated_tables_and_comments() {
    // Given: コメント・ルーティング・エージェント・既存の二つのプロファイル
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    let original = "version = 2 # comment\n\n[routing]\nroutes = { fast = [{ profile = 'other' }] }\n\n[agents.worker]\nlogical_model = 'fast'\n\n[providers.other]\nprovider_type = 'anthropic'\ncredential = { type = 'env', var = 'OTHER_KEY' } # keep\n\n[providers.third]\nprovider_type = 'openrouter'\n";
    std::fs::write(&path, original).unwrap();
    // When: 新しいプロファイルだけを追加する
    save_openai_compatible_provider(&path, &input()).expect("preserving save succeeds");
    // Then: 無関係な全バイトが保たれ、三つのプロファイルを読み込める
    assert!(std::fs::read_to_string(path).unwrap().starts_with(original));
    assert_eq!(load(tmp.path()).providers.len(), 3);
}

#[test]
fn save_same_name_profile_replaces_entry_and_drops_stale_credential() {
    // Given: 同名プロファイルに古いキーリング参照がある
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    std::fs::write(&path, "version = 2\n[providers.local]\ncredential = { type = 'keyring', service = 'stale-service', account = 'old' }\n").unwrap();
    // When: 同名プロファイルを保存する
    save_openai_compatible_provider(&path, &input()).expect("replacement save succeeds");
    // Then: 古い参照が消え、指定された五つのキーだけになる
    let text = std::fs::read_to_string(path).unwrap();
    assert!(!text.contains("keyring"));
    assert!(!text.contains("stale-service"));
    assert_eq!(text.matches("[providers.local]").count(), 1);
    let table: toml::Value = toml::from_str(&text).unwrap();
    let keys: Vec<_> = table["providers"]["local"]
        .as_table()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["api_key_env", "base_url", "default_model", "models", "type"]
    );
    assert_eq!(load(tmp.path()).providers["local"].default_model, "model-a");
}

#[test]
fn save_rejects_plaintext_like_api_key_env_and_leaves_file_untouched() {
    for value in [
        "sk-abc123",
        "openai_key",
        "",
        "KEY WITH SPACE",
        "lowercase_key",
        "1_KEY",
    ] {
        for original in [None, Some("version = 2\n# untouched\n")] {
            // Given: 不正な環境変数名と、新規または既存ファイル
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("evorch.toml");
            if let Some(text) = original {
                std::fs::write(&path, text).unwrap();
            }
            let mut candidate = input();
            candidate.credential = config::ProviderCredentialInput::Env { var: value.into() };
            // When: 保存を要求する
            let result = save_openai_compatible_provider(&path, &candidate);
            // Then: フィールドエラーになりファイルは変更されない
            assert_field(result, "providers.local.api_key_env");
            assert_eq!(std::fs::read_to_string(path).ok().as_deref(), original);
        }
    }
}

#[test]
fn save_rejects_invalid_name_base_url_models_default_model() {
    for (field, value) in [
        ("name", "has space"),
        ("name", "dot.name"),
        ("name", ""),
        ("base_url", "api.example.com"),
        ("base_url", ""),
        ("models", ""),
        ("models", "   "),
        ("default_model", "missing"),
    ] {
        // Given: 一つのフィールドが不正で、読み取りもできないパス
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing-parent/evorch.toml");
        let mut candidate = input();
        match field {
            "name" => candidate.name = value.into(),
            "base_url" => candidate.base_url = value.into(),
            "models" => candidate.models = vec![value.into()],
            "default_model" => candidate.default_model = value.into(),
            _ => unreachable!(),
        }
        // When: 保存を要求する
        let result = save_openai_compatible_provider(&path, &candidate);
        // Then: I/O より先に入力エラーになる
        assert_field(result, &format!("providers.{}.{field}", candidate.name));
        assert!(!path.exists());
    }
}

#[test]
fn save_refuses_non_v2_file() {
    for version in [0, 1, 3] {
        // Given: 現行以外のバージョン
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evorch.toml");
        let original = format!("version = {version}\n# untouched\n");
        std::fs::write(&path, &original).unwrap();
        // When: 保存を要求する
        let result = save_openai_compatible_provider(&path, &input());
        // Then: 移行せず拒否する
        assert!(
            matches!(result, Err(ConfigError::UnsupportedVersion { found, current: 2 }) if found == version),
            "expected UnsupportedVersion, got {result:?}"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}

#[test]
fn save_result_round_trips_through_config_load_stably() {
    // Given: 一度保存した設定
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    save_openai_compatible_provider(&path, &input()).expect("initial save succeeds");
    let first = load(tmp.path());
    let bytes = std::fs::read(&path).unwrap();
    // When: 同じ入力で再保存する
    save_openai_compatible_provider(&path, &input()).expect("repeat save succeeds");
    // Then: ロード結果とファイル内容は変わらない
    assert_eq!(load(tmp.path()), first);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn save_rejects_invalid_existing_document_without_writing() {
    for original in [
        "[broken",
        "version = '2'\n",
        "version = -1\n",
        "version = 4294967296\n",
        "version = 2\nproviders = 5\n",
        "version = 2\n[providers.other]\napi_key = 'secret'\n",
        "version = 2\n[metrics]\nenabled = 'wrong-type'\n",
    ] {
        // Given: 不正 TOML・バージョン・既存設定
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evorch.toml");
        std::fs::write(&path, original).unwrap();
        // When: 保存を要求する
        let result = save_openai_compatible_provider(&path, &input());
        // Then: 検証で拒否し、一時ファイルすら作らない
        assert!(matches!(
            result,
            Err(ConfigError::InvalidField { .. }
                | ConfigError::Parse { .. }
                | ConfigError::Migration(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!tmp.path().join("evorch.toml.tmp").exists());
    }
}

#[test]
fn validation_reports_first_invalid_field_in_order() {
    // Given: 全フィールドが不正
    let mut candidate = OpenAiCompatibleProviderInput {
        name: String::new(),
        base_url: String::new(),
        credential: config::ProviderCredentialInput::Env { var: String::new() },
        models: vec![],
        excluded_models: vec![],
        default_model: String::new(),
    };
    for field in ["name", "base_url", "api_key_env", "models", "default_model"] {
        // When: 先頭の不正フィールドを検証する
        let result = validate_openai_compatible_provider_input(&candidate);
        // Then: 仕様順に一つだけ報告される
        assert_field(result, &format!("providers.{}.{field}", candidate.name));
        match field {
            "name" => candidate.name = "Local_1-test".into(),
            "base_url" => candidate.base_url = "https://example.com".into(),
            "api_key_env" => {
                candidate.credential = config::ProviderCredentialInput::Env {
                    var: "_KEY_1".into(),
                }
            }
            "models" => candidate.models = vec!["model-a".into()],
            "default_model" => candidate.default_model = "model-a".into(),
            _ => unreachable!(),
        }
    }
    validate_openai_compatible_provider_input(&candidate).unwrap();
}
