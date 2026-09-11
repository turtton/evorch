use super::*;

#[test]
#[ignore = "requires an offscreen GPU adapter"]
fn capture_both_settings_tabs() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    open_valid_settings(&mut harness);
    // When
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    frame
        .save_png(std::path::Path::new("/tmp/opencode/w-b-openai.png"))
        .unwrap();
    harness.click_label("Cancel");
    harness.run();
    harness.click_label("+ Add Codex subscription");
    harness.run();
    // Then
    assert!(harness.has_label(CODEX_LOGIN_BUTTON));
    let Some(frame) = gui::evidence::capture_or_skip(&mut harness) else {
        return;
    };
    frame
        .save_png(std::path::Path::new("/tmp/opencode/w-b-codex.png"))
        .unwrap();
}

#[test]
fn openai_editor_shows_form_without_codex_login() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    // When
    open_valid_settings(&mut harness);
    // Then
    assert!(harness.has_label("Name"));
    assert!(harness.has_label("Base URL"));
    assert!(!harness.has_label(CODEX_LOGIN_BUTTON));
}

#[test]
fn codex_tab_hides_openai_grid_and_shows_login() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    open_valid_settings(&mut harness);
    // When
    harness.click_label("Cancel");
    harness.run();
    harness.click_label("+ Add Codex subscription");
    harness.run();
    // Then
    assert!(!harness.has_label("Base URL"));
    assert!(harness.has_label(CODEX_LOGIN_BUTTON));
}

#[test]
fn refresh_models_sits_below_configured_models() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    // When
    open_valid_settings(&mut harness);
    // Then
    let base = harness.label_rects("Base URL")[0];
    let refresh = harness.label_rects("Refresh models")[0];
    let models = harness.label_rects("Configured models")[0];
    assert!(base.max.y < refresh.min.y);
    assert!(models.max.y < refresh.min.y);
}

#[test]
fn api_key_field_is_password_and_saves_to_credential_store() {
    use sandbox::CredentialStore;
    // Given
    let temp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        sandbox::FileCredentialStore::open(temp.path().join("credentials")).unwrap(),
    );
    let state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_provider_settings_path(temp.path().join("evorch.toml"))
        .with_credential_store(store.clone());
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    let model = harness.state_mut().provider_settings_mut();
    model.open = true;
    model.add(gui::model::provider_settings::ProviderKind::OpenAiCompatible);
    let model = model.openai_mut().unwrap();
    model.base_url = "https://example.com/v1".into();
    model.api_key_input = "sk-test".into();
    model.models = vec![config::types::provider::ModelEntryConfig::enabled(
        "model-a",
    )];
    model.default_model = "model-a".into();
    harness.run();
    assert!(!harness.has_label("sk-test"));
    // When
    harness.click_label("Save");
    for _ in 0..500 {
        harness.step();
        if harness.state().provider_settings().editor.is_none() {
            break;
        }
        std::thread::yield_now();
    }
    // Then
    assert_eq!(
        store.get("openai-compat").unwrap().unwrap().expose(),
        "sk-test"
    );
    assert!(harness.state().provider_settings().editor.is_none());
    let text = std::fs::read_to_string(temp.path().join("evorch.toml")).unwrap();
    assert!(text.contains("type = \"keyring\""));
    assert!(!text.contains("sk-test"));
}

#[test]
fn save_in_keyring_mode_without_store_shows_error_and_writes_nothing() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let mut harness = workbench_with_config_path(temp.path());
    open_valid_settings(&mut harness);
    harness
        .state_mut()
        .provider_settings_mut()
        .openai_mut()
        .unwrap()
        .credential_mode = gui::model::provider_settings::CredentialMode::Keyring;
    // When
    harness.click_label("Save");
    finish_save(&mut harness);
    // Then
    assert_eq!(
        harness.state().provider_settings().error.as_deref(),
        Some("Credential store unavailable; use environment-variable mode")
    );
    assert!(!temp.path().join("evorch.toml").exists());
}

#[test]
fn models_fetch_uses_stored_secret_in_keyring_mode() {
    use gui::model::provider_settings::ModelsFetchState;
    use sandbox::CredentialStore;
    // Given
    let server = mock_openai::StreamingMockOpenAi::spawn_with_models(
        vec![],
        mock_openai::WriteMode::default(),
        vec!["model-a".into()],
    );
    let temp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(sandbox::FileCredentialStore::open(temp.path()).unwrap());
    store
        .set(
            "openai-compat",
            &sandbox::Secret::from("sk-test".to_owned()),
        )
        .unwrap();
    let mut model = OpenAiEditorModel {
        base_url: server.base_url(),
        ..Default::default()
    };
    // When
    model.start_models_fetch_with_store(Some(store));
    let result = model
        .models_rx
        .take()
        .unwrap()
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(result).unwrap();
    model.models_rx = Some(rx);
    model.poll_models();
    // Then
    assert_eq!(model.models_fetch_state, ModelsFetchState::Loaded);
    assert!(
        server
            .recorded_requests()
            .iter()
            .any(|r| r.authorization.as_deref() == Some("Bearer sk-test"))
    );
}
