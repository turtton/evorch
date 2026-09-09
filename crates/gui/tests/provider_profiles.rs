use config::{Config, ProviderProfileConfig, ProviderTypeConfig};
use gui::model::provider_settings::{ProfileEditor, ProviderKind, ProviderSettingsModel};

#[test]
fn seed_lists_all_profiles_sorted() {
    // Given
    let mut config = Config::default();
    for name in ["z", "a", "m"] {
        config
            .providers
            .insert(name.into(), ProviderProfileConfig::default());
    }
    // When
    let model = ProviderSettingsModel::seed_from_config(&config);
    // Then
    assert_eq!(
        model
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "m", "z"]
    );
    assert!(model.editor.is_none());
}

#[test]
fn add_codex_editor_defaults_account_to_profile_name() {
    // Given
    let mut model = ProviderSettingsModel::default();
    // When
    model.add(ProviderKind::CodexSubscription);
    // Then
    let Some(ProfileEditor::Codex(editor)) = &model.editor else {
        panic!("Codex editor")
    };
    assert_eq!(editor.account, editor.name);
    assert_eq!(editor.auth.credential_account, editor.name);
}

#[test]
fn edit_existing_openai_profile_prefills_form() {
    // Given
    let mut config = Config::default();
    config.providers.insert(
        "work".into(),
        ProviderProfileConfig {
            provider_type: ProviderTypeConfig::OpenAiCompatible,
            base_url: "https://example.com/v1".into(),
            models: vec![config::types::provider::ModelEntryConfig::enabled("model")],
            default_model: "model".into(),
            ..Default::default()
        },
    );
    let mut model = ProviderSettingsModel::seed_from_config(&config);
    // When
    model.edit("work");
    // Then
    let Some(ProfileEditor::OpenAiCompatible(editor)) = &model.editor else {
        panic!("OpenAI editor")
    };
    assert_eq!(editor.name, "work");
    assert_eq!(editor.base_url, "https://example.com/v1");
    assert_eq!(editor.default_model, "model");
}

#[test]
fn delete_removes_profile_and_refreshes_list() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evorch.toml");
    config::save_codex_provider(
        &path,
        &config::CodexProviderInput {
            name: "work".into(),
            account: "work".into(),
            models: vec![],
            default_model: String::new(),
        },
    )
    .unwrap();
    // When
    config::delete_provider(&path, "work").unwrap();
    let config = Config::load(&config::LoadOptions {
        project_dir: Some(tmp.path().into()),
        user_config_dir: Some(tmp.path().join("empty")),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    // Then
    assert!(
        ProviderSettingsModel::seed_from_config(&config)
            .profiles
            .is_empty()
    );
}

#[test]
fn codex_editors_keep_distinct_credential_accounts() {
    // Given
    let mut config = Config::default();
    for name in ["personal", "work"] {
        config.providers.insert(
            name.into(),
            ProviderProfileConfig {
                provider_type: ProviderTypeConfig::OpenAiCodex,
                credential: config::CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: name.into(),
                },
                ..Default::default()
            },
        );
    }
    let mut model = ProviderSettingsModel::seed_from_config(&config);
    // When
    model.edit("personal");
    let personal = model.codex_mut().unwrap().auth.credential_account.clone();
    model.edit("work");
    // Then
    assert_eq!(personal, "personal");
    assert_eq!(model.codex_mut().unwrap().auth.credential_account, "work");
}
