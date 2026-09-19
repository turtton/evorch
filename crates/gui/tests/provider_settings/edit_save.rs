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
    config::save_openai_compatible_provider(
        &root.join("evorch.toml"),
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
        .with_provider_settings_path(root.join("evorch.toml"))
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
    config::save_codex_provider(
        &temp.path().join("evorch.toml"),
        &config::CodexProviderInput {
            name: "A".into(),
            account: "oauth-account".into(),
            models: vec![],
            default_model: String::new(),
        },
    )
    .unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    *harness.state_mut().provider_settings_mut() =
        ProviderSettingsModel::seed_from_config(&load_config(temp.path()));
    harness.state_mut().provider_settings_mut().edit("A");
    harness
        .state_mut()
        .provider_settings_mut()
        .codex_mut()
        .unwrap()
        .name = "B".into();
    // When
    harness.state_mut().submit_provider_settings();
    finish_save(&mut harness);
    // Then
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
