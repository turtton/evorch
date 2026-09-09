use super::*;

fn seeded(root: &std::path::Path) -> HeadlessWorkbench<DemoSource> {
    for name in ["personal", "work"] {
        config::save_codex_provider(
            &root.join("evorch.toml"),
            &config::CodexProviderInput {
                name: name.into(),
                account: name.into(),
                models: vec![],
                default_model: String::new(),
            },
        )
        .unwrap();
    }
    let mut harness = workbench_with_config_path(root);
    *harness.state_mut().provider_settings_mut() =
        ProviderSettingsModel::seed_from_config(&load_config(root));
    harness.state_mut().open_provider_settings();
    harness.run();
    harness
}

#[test]
fn settings_lists_two_profiles_with_edit_and_delete_actions() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    // When
    let harness = seeded(tmp.path());
    // Then
    assert!(harness.has_label("personal"));
    assert!(harness.has_label("work"));
    assert_eq!(harness.label_rects("Edit").len(), 2);
    assert_eq!(harness.label_rects("Delete").len(), 2);
}

#[test]
fn add_codex_opens_editor_with_sign_in_button() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let mut harness = seeded(tmp.path());
    // When
    harness.click_label("+ Add Codex subscription");
    harness.run();
    // Then
    assert!(harness.has_label(CODEX_LOGIN_BUTTON));
    assert!(harness.has_label("Account"));
    assert!(!harness.has_label("Base URL"));
}

#[test]
fn save_new_profile_appends_to_list_and_config() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let mut harness = seeded(tmp.path());
    harness.click_label("+ Add OpenAI-compatible");
    harness.run();
    let editor = harness
        .state_mut()
        .provider_settings_mut()
        .openai_mut()
        .unwrap();
    editor.name = "new-profile".into();
    editor.base_url = "https://example.com/v1".into();
    editor.credential_mode = gui::model::provider_settings::CredentialMode::Env;
    editor.api_key_env = "API_KEY".into();
    editor.models = vec![config::types::provider::ModelEntryConfig::enabled("model")];
    editor.default_model = "model".into();
    harness.run();
    // When
    harness.click_label("Save");
    finish_save(&mut harness);
    // Then
    assert!(harness.has_label("new-profile"));
    assert_eq!(load_config(tmp.path()).providers.len(), 3);
    assert!(
        load_config(tmp.path())
            .providers
            .contains_key("new-profile")
    );
}

#[test]
fn confirm_delete_removes_row_and_toml_entry() {
    // Given
    let tmp = tempfile::tempdir().unwrap();
    let mut harness = seeded(tmp.path());
    harness.state_mut().provider_settings_mut().confirm_delete = Some("personal".into());
    harness.step();
    harness.run();
    assert!(load_config(tmp.path()).providers.contains_key("personal"));
    // When
    harness.click_label("Confirm delete");
    for _ in 0..1000 {
        harness.step();
        if harness.state().provider_settings().profiles.len() == 1 {
            break;
        }
        std::thread::yield_now();
    }
    harness.run();
    // Then
    assert!(
        !harness.has_label("personal"),
        "{:?}",
        harness.state().provider_settings()
    );
    assert!(harness.has_label("work"));
    assert!(!load_config(tmp.path()).providers.contains_key("personal"));
}
