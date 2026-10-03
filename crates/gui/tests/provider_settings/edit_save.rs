use super::*;
use sandbox::CredentialStore;
use std::sync::Arc;

pub(super) fn keyring_editor(
    root: &std::path::Path,
) -> (
    HeadlessWorkbench<DemoSource>,
    Arc<sandbox::FileCredentialStore>,
) {
    let store = Arc::new(sandbox::FileCredentialStore::open(root.join("credentials")).unwrap());
    store
        .set("acct-A", &sandbox::Secret::from("old-token".to_owned()))
        .unwrap();
    std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).expect("config directory");
    config::save_openai_compatible_provider(
        &config::project_main_config_path(root),
        &config::OpenAiCompatibleProviderInput {
            name: "A".into(),
            provider_type: ProviderTypeConfig::OpenAiCompatible,
            base_url: "https://example.invalid/v1".into(),
            credential: config::ProviderCredentialInput::Keyring {
                service: "evorch".into(),
                account: "acct-A".into(),
            },
            models: vec![config::ModelEntryConfig::enabled("model")],
            excluded_models: vec![],
            default_model: "model".into(),
        },
    )
    .unwrap();
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_settings_load_options(config::LoadOptions {
            project_dir: Some(root.to_path_buf()),
            user_config_dir: Some(root.join("isolated-user-config")),
            read_env: false,
            ..Default::default()
        })
        .with_provider_settings_path(config::project_main_config_path(root))
        .with_provider_settings(ProviderSettingsModel::seed_from_config(&load_config(root)))
        .with_credential_store(store.clone());
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.state_mut().open_provider_settings();
    harness.run();
    harness.click_label("Edit");
    harness.run();
    harness.step();
    (harness, store)
}

#[test]
fn replaces_profile_when_renaming_with_existing_token() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, store) = keyring_editor(temp.path());
    harness
        .state_mut()
        .provider_settings_mut()
        .openai_mut()
        .unwrap()
        .name = "B".into();
    harness.run();
    // When
    harness.click_label("Save");
    finish_save(&mut harness);
    // Then
    let cfg = load_config(temp.path());
    assert_eq!(
        cfg.providers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["B"]
    );
    assert_eq!(store.get("B").unwrap().unwrap().expose(), "old-token");
    assert!(store.get("acct-A").unwrap().is_none());
    assert!(harness.state().provider_settings().error.is_none());
    assert!(harness.state().provider_settings().open);
    assert!(harness.state().provider_settings().editor.is_none());
    assert!(harness.has_label("B"));
    assert!(!harness.has_label("A"));
    assert!(harness.has_label("Edit"));
    assert!(!harness.has_label("Save"));
}

#[test]
fn failed_edit_save_keeps_editor_open() {
    // Given: an existing keyring profile has lost its stored credential.
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, store) = keyring_editor(temp.path());
    store.delete("acct-A").unwrap();
    let path = config::project_main_config_path(temp.path());
    let before = std::fs::read_to_string(&path).unwrap();
    // When: saving fails in the credential worker.
    harness.click_label("Save");
    finish_save(&mut harness);
    // Then: the error and editable settings stay visible, and config is unchanged.
    assert!(harness.state().provider_settings().open);
    assert!(harness.state().provider_settings().editor.is_some());
    let error = harness
        .state()
        .provider_settings()
        .error
        .as_deref()
        .expect("inline error");
    assert!(error.contains("Enter an API key before saving"), "{error}");
    assert!(harness.has_label(error));
    assert!(harness.has_label("Save"));
    assert!(!harness.has_label("Edit"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn overwrites_original_account_when_typing_replacement_token() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, store) = keyring_editor(temp.path());
    harness
        .state_mut()
        .provider_settings_mut()
        .openai_mut()
        .unwrap()
        .api_key_input = "new-token".into();
    // When
    harness.state_mut().submit_provider_settings();
    finish_save(&mut harness);
    // Then
    assert_eq!(store.get("acct-A").unwrap().unwrap().expose(), "new-token");
    assert!(store.get("A").unwrap().is_none());
}

#[test]
fn replaces_profile_when_renaming_with_new_token() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, store) = keyring_editor(temp.path());
    let editor = harness
        .state_mut()
        .provider_settings_mut()
        .openai_mut()
        .unwrap();
    editor.name = "B".into();
    editor.api_key_input = "new-token".into();
    // When
    harness.state_mut().submit_provider_settings();
    finish_save(&mut harness);
    // Then
    let cfg = load_config(temp.path());
    assert_eq!(
        cfg.providers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["B"]
    );
    assert_eq!(store.get("B").unwrap().unwrap().expose(), "new-token");
    assert!(store.get("acct-A").unwrap().is_none());
}

#[test]
fn replaces_profile_when_renaming_env_profile() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    open_valid_settings(&mut harness);
    harness.state_mut().submit_provider_settings();
    finish_save(&mut harness);
    harness.state_mut().provider_settings_mut().edit("local");
    harness
        .state_mut()
        .provider_settings_mut()
        .openai_mut()
        .unwrap()
        .name = "B".into();
    // When
    harness.state_mut().submit_provider_settings();
    finish_save(&mut harness);
    // Then
    assert_eq!(
        load_config(temp.path())
            .providers
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["B"]
    );
}

#[test]
fn replaces_profile_when_renaming_codex_profile() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(config::PROJECT_CONFIG_DIR))
        .expect("config directory");
    config::save_codex_provider(
        &config::project_main_config_path(temp.path()),
        &config::CodexProviderInput {
            name: "A".into(),
            account: "oauth-account".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            models: vec![],
            default_model: String::new(),
        },
    )
    .unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    *harness.state_mut().provider_settings_mut() =
        ProviderSettingsModel::seed_from_config(&load_config(temp.path()));
    harness.state_mut().open_provider_settings();
    harness.run();
    harness.click_label("Edit");
    harness.run();
    harness
        .state_mut()
        .provider_settings_mut()
        .codex_mut()
        .unwrap()
        .name = "B".into();
    harness.run();
    // When: saving an edit from the provider list.
    harness.click_label("Save");
    finish_save(&mut harness);
    // Then: the refreshed provider list remains open with the renamed profile.
    assert!(harness.state().provider_settings().error.is_none());
    assert!(harness.state().provider_settings().open);
    assert!(harness.state().provider_settings().editor.is_none());
    assert!(harness.has_label("Provider settings"));
    assert!(harness.has_label("B"));
    assert!(!harness.has_label("A"));
    assert!(harness.has_label("Edit"));
    assert!(!harness.has_label("Save"));
    let cfg = load_config(temp.path());
    assert_eq!(
        cfg.providers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["B"]
    );
    assert_eq!(
        cfg.providers["B"].credential,
        CredentialRefConfig::Keyring {
            service: "evorch".into(),
            account: "oauth-account".into()
        }
    );
}

#[test]
fn codex_editor_saves_context_override_and_retains_model_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let path = config::project_main_config_path(temp.path());
    std::fs::create_dir_all(temp.path().join(config::PROJECT_CONFIG_DIR))
        .expect("config directory");
    let mut model = config::ModelEntryConfig::enabled("gpt-6-astra");
    model.metadata_source = Some(config::MetadataSource::ModelsDev);
    model.metadata_ref = Some("openai/gpt-6-astra".into());
    model.input_price = Some(2.0);
    config::save_codex_provider(
        &path,
        &config::CodexProviderInput {
            name: "work".into(),
            account: "oauth-account".into(),
            base_url: "https://example.com/custom-codex".into(),
            models: vec![model.clone()],
            default_model: model.id.clone(),
        },
    )
    .unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    *harness.state_mut().provider_settings_mut() =
        ProviderSettingsModel::seed_from_config(&load_config(temp.path()));
    harness.state_mut().provider_settings_mut().edit("work");
    let editor = harness
        .state_mut()
        .provider_settings_mut()
        .codex_mut()
        .unwrap();
    assert_eq!(editor.model_entries["gpt-6-astra"], model);
    editor
        .model_entries
        .get_mut("gpt-6-astra")
        .unwrap()
        .context_window = Some(272_000);
    harness.state_mut().submit_provider_settings();
    finish_save(&mut harness);
    model.context_window = Some(272_000);
    let saved = &load_config(temp.path()).providers["work"];
    assert_eq!(saved.models, vec![model]);
    assert_eq!(saved.base_url, "https://example.com/custom-codex");
}
