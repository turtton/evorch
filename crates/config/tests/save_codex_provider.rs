use config::{CodexProviderInput, Config, CredentialRefConfig, LoadOptions, ProviderTypeConfig};

fn codex(name: &str) -> CodexProviderInput {
    CodexProviderInput {
        name: name.into(),
        account: name.into(),
        models: vec!["gpt-5-codex".into()],
        default_model: "gpt-5-codex".into(),
    }
}

fn load(dir: &std::path::Path) -> Config {
    Config::load(&LoadOptions {
        project_dir: Some(dir.into()),
        user_config_dir: Some(dir.join("empty")),
        read_env: false,
        ..LoadOptions::default()
    })
    .unwrap()
}

fn openai(name: &str) -> config::OpenAiCompatibleProviderInput {
    config::OpenAiCompatibleProviderInput {
        name: name.into(),
        base_url: "https://example.com/v1".into(),
        credential: config::ProviderCredentialInput::Env {
            var: "API_KEY".into(),
        },
        models: vec!["model".into()],
        excluded_models: vec![],
        default_model: "model".into(),
    }
}

#[test]
fn save_codex_provider_round_trips() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    // When
    config::save_codex_provider(&tmp.path().join("evorch.toml"), &codex("work")).unwrap();
    // Then
    let cfg = load(tmp.path());
    let profile = &cfg.providers["work"];
    assert_eq!(profile.provider_type, ProviderTypeConfig::OpenAiCodex);
    assert_eq!(
        profile.api_protocol,
        config::ApiProtocolConfig::OpenAiCodexResponses
    );
    assert_eq!(
        profile.credential,
        CredentialRefConfig::Keyring {
            service: "evorch".into(),
            account: "work".into()
        }
    );
    assert_eq!(profile.default_model, "gpt-5-codex");
    assert_eq!(profile.models, ["gpt-5-codex"]);
}

#[test]
fn three_profiles_of_mixed_types_coexist() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    for name in ["local", "remote"] {
        config::save_openai_compatible_provider(&path, &openai(name)).unwrap();
    }
    // When
    config::save_codex_provider(&path, &codex("work")).unwrap();
    // Then
    assert_eq!(
        load(tmp.path())
            .providers
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["local", "remote", "work"]
    );
}

#[test]
fn delete_provider_removes_only_target() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    config::save_openai_compatible_provider(&path, &openai("local")).unwrap();
    config::save_codex_provider(&path, &codex("work")).unwrap();
    // When
    config::delete_provider(&path, "work").unwrap();
    // Then
    assert_eq!(
        load(tmp.path())
            .providers
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["local"]
    );
}

#[test]
fn switching_credential_modes_drops_stale_env_key_on_save() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    config::save_openai_compatible_provider(&path, &openai("work")).unwrap();
    // When
    config::save_codex_provider(&path, &codex("work")).unwrap();
    // Then
    let doc: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(doc["providers"]["work"].get("api_key_env").is_none());
    assert_eq!(
        doc["providers"]["work"]["base_url"].as_str(),
        Some("https://chatgpt.com/backend-api/codex")
    );
    assert_eq!(
        load(tmp.path()).providers["work"].provider_type,
        ProviderTypeConfig::OpenAiCodex
    );
}
