use config::{CredentialRefConfig, ModelEntryConfig, ProviderProfileConfig, ProviderTypeConfig};
use gui::model::provider_settings::{ProfileEditor, ProviderKind, ProviderSettingsModel};

#[test]
fn claude_api_editor_preserves_environment_credentials_and_model_metadata() {
    let mut config = config::Config::default();
    let mut entry = ModelEntryConfig::enabled("claude-sonnet-4-6");
    entry.context_window = Some(200_000);
    let profile = ProviderProfileConfig {
        provider_type: ProviderTypeConfig::Anthropic,
        base_url: config::types::provider::CLAUDE_DEFAULT_BASE_URL.into(),
        credential: CredentialRefConfig::Env {
            var: "TEAM_ANTHROPIC_KEY".into(),
        },
        models: vec![entry.clone()],
        default_model: entry.id.clone(),
        ..Default::default()
    };
    config.providers.insert("team-api".into(), profile);
    let mut settings = ProviderSettingsModel::seed_from_config(&config);
    settings.edit("team-api");
    let editor = settings.openai_mut().unwrap();
    let input = editor.to_input();
    assert_eq!(input.provider_type, ProviderTypeConfig::Anthropic);
    assert_eq!(input.models, vec![entry]);
    assert!(
        matches!(input.credential, config::ProviderCredentialInput::Env { var } if var == "TEAM_ANTHROPIC_KEY")
    );
    assert!(editor.api_key_input.is_empty());
}

#[test]
fn subscription_editors_preserve_profile_account_and_models_on_save() {
    for (kind, provider_type) in [
        (
            ProviderKind::ClaudeSubscription,
            ProviderTypeConfig::AnthropicSubscription,
        ),
        (ProviderKind::Cursor, ProviderTypeConfig::Cursor),
    ] {
        let mut settings = ProviderSettingsModel::default();
        settings.add(kind);
        let editor = settings.subscription_mut().unwrap();
        assert!(!editor.models.models.is_empty());
        let mut entry = editor.models.models[0].clone();
        entry.context_window = Some(131_072);
        let name = editor.models.name.clone();
        let mut config = config::Config::default();
        config.providers.insert(
            name.clone(),
            ProviderProfileConfig {
                provider_type,
                credential: CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: "separate-account".into(),
                },
                base_url: editor.models.base_url.clone(),
                models: vec![entry.clone()],
                default_model: entry.id.clone(),
                excluded_models: vec!["blocked-model".into()],
                ..Default::default()
            },
        );
        let mut settings = ProviderSettingsModel::seed_from_config(&config);
        settings.edit(&name);
        let ProfileEditor::Subscription(editor) = settings.editor.as_ref().unwrap() else {
            panic!("native subscription editor");
        };
        let input = editor.to_input();
        assert_eq!(input.account, "separate-account");
        assert_eq!(input.models, vec![entry]);
        assert_eq!(input.provider_type, provider_type);
        assert_eq!(input.excluded_models, vec!["blocked-model"]);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        config::save_subscription_provider_edit(&path, &input, None).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("access_token"));
        assert!(!saved.contains("refresh_token"));
        config::delete_provider(&path, &input.name).unwrap();
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("separate-account")
        );
    }
}

fn settings_harness(
    settings: ProviderSettingsModel,
) -> egui_kittest::Harness<
    'static,
    (
        ProviderSettingsModel,
        Option<gui::panes::provider_settings::ProviderSettingsAction>,
    ),
> {
    egui_kittest::Harness::builder()
        .with_size(egui::vec2(1200.0, 1400.0))
        .build_ui_state(
            |ui, state| {
                gui::theme::install(ui.ctx());
                if let Some(action) = gui::panes::provider_settings::provider_settings_modal(
                    ui.ctx(),
                    &mut state.0,
                    &gui::model::codex_auth::CodexAuthModel::default(),
                ) {
                    state.1 = Some(action);
                }
            },
            (settings, None),
        )
}

#[test]
fn settings_buttons_open_each_native_editor_and_api_key_editor() {
    use egui_kittest::kittest::Queryable;
    for (label, kind) in [
        ("+ Add Claude API", ProviderKind::ClaudeApi),
        (
            "+ Add Claude subscription",
            ProviderKind::ClaudeSubscription,
        ),
        ("+ Add Cursor", ProviderKind::Cursor),
    ] {
        let mut harness = settings_harness(ProviderSettingsModel::default());
        harness.run();
        harness.get_by_label(label).click();
        harness.run();
        match kind {
            ProviderKind::ClaudeApi => {
                assert_eq!(
                    harness.state().0.openai().unwrap().provider_type,
                    ProviderTypeConfig::Anthropic
                );
                assert!(harness.query_by_label("API key").is_some());
            }
            _ => {
                let Some(ProfileEditor::Subscription(editor)) = &harness.state().0.editor else {
                    panic!("subscription editor");
                };
                assert!(
                    harness
                        .query_by_label(&format!("Sign in to {}", editor.service()))
                        .is_some()
                );
            }
        }
    }
}

#[test]
fn manual_claude_callback_is_password_masked_and_save_is_disabled_during_login() {
    use egui_kittest::kittest::{NodeT, Queryable};
    let mut settings = ProviderSettingsModel::default();
    settings.add(ProviderKind::ClaudeSubscription);
    let editor = settings.subscription_mut().unwrap();
    editor.auth =
        gui::model::subscription_provider::SubscriptionAuthState::Waiting { manual_code: true };
    editor.code_input = "sensitive-login-code".into();
    editor.authorize_url = Some("https://claude.ai/oauth/authorize".into());
    let mut harness = settings_harness(settings);
    harness.run_steps(8);
    assert!(harness.get_by_label("Save").accesskit_node().is_disabled());
    harness.get_by_label("Complete sign-in").click();
    harness.run_steps(4);
    assert_eq!(
        harness.state().1,
        Some(gui::panes::provider_settings::ProviderSettingsAction::CompleteSubscriptionLogin)
    );
    assert!(harness.query_by_label("sensitive-login-code").is_none());
    assert!(
        harness
            .get_by_label("Authorization code / callback URL")
            .accesskit_node()
            .value()
            .is_none_or(|value| !value.contains("sensitive-login-code"))
    );
    harness.get_by_label("Cancel sign-in").click();
    harness.run_steps(4);
    assert_eq!(
        harness.state().1,
        Some(gui::panes::provider_settings::ProviderSettingsAction::CancelSubscriptionLogin)
    );
}

#[test]
fn fetched_subscription_models_can_be_selected_and_applied() {
    use egui_kittest::kittest::Queryable;
    let mut settings = ProviderSettingsModel::default();
    settings.add(ProviderKind::Cursor);
    let editor = settings.subscription_mut().unwrap();
    editor.models.available_models = Some(vec![
        config::types::provider::CURSOR_DEFAULT_MODEL.into(),
        "provider-new-model".into(),
    ]);
    editor.models.models_fetch_state = gui::model::provider_settings::ModelsFetchState::Loaded;
    let mut harness = settings_harness(settings);
    harness.run();
    // Nested scroll areas finish their sizing pass before dispatching pointer input.
    harness.run_steps(8);
    harness.get_by_label("provider-new-model").click();
    harness.run();
    harness.get_by_label("Apply selected (1)").click();
    harness.run();
    let Some(ProfileEditor::Subscription(editor)) = &harness.state().0.editor else {
        panic!("subscription editor");
    };
    assert!(
        editor
            .models
            .models
            .iter()
            .any(|m| m.id == "provider-new-model" && m.enabled)
    );
    assert_eq!(
        editor.models.default_model,
        config::types::provider::CURSOR_DEFAULT_MODEL
    );
}

#[test]
fn renaming_existing_subscription_preserves_stored_account_reference() {
    for kind in [ProviderKind::ClaudeSubscription, ProviderKind::Cursor] {
        let mut settings = ProviderSettingsModel::default();
        settings.add(kind);
        let editor = settings.subscription_mut().unwrap();
        let original = editor.models.name.clone();
        editor.models.original_name = Some(original.clone());
        editor.models.name = "renamed".into();
        editor.rename_profile(&original);
        assert_eq!(editor.to_input().account, original);
        editor.models.original_name = None;
        editor.models.name = "new-profile".into();
        editor.rename_profile(&original);
        assert_eq!(editor.to_input().account, "new-profile");
    }
}

#[test]
fn cursor_api_key_ui_masks_input_dispatches_login_and_disables_controls_while_busy() {
    use egui_kittest::kittest::{NodeT, Queryable};
    use gui::model::subscription_provider::{SubscriptionAuthMethod, SubscriptionAuthState};
    use gui::panes::provider_settings::ProviderSettingsAction;
    let mut settings = ProviderSettingsModel::default();
    settings.add(ProviderKind::Cursor);
    let mut harness = settings_harness(settings);
    harness.run_steps(8);
    assert!(harness.query_by_label("Sign in to Cursor").is_some());
    harness.get_by_label("API key").click();
    harness.run_steps(4);
    assert_eq!(
        match harness.state().0.editor.as_ref().unwrap() {
            ProfileEditor::Subscription(editor) => editor.auth_method,
            _ => panic!("subscription editor"),
        },
        SubscriptionAuthMethod::ApiKey
    );
    assert!(
        harness
            .get_by_label("Authenticate with API key")
            .accesskit_node()
            .is_disabled()
    );
    harness
        .state_mut()
        .0
        .subscription_mut()
        .unwrap()
        .api_key_input = "cursor-key-secret".into();
    harness.run_steps(4);
    assert!(harness.query_by_label("cursor-key-secret").is_none());
    assert!(
        harness
            .get_by_label("Cursor API key")
            .accesskit_node()
            .value()
            .is_none_or(|value| !value.contains("cursor-key-secret"))
    );
    harness.get_by_label("Authenticate with API key").click();
    harness.run_steps(4);
    assert_eq!(
        harness.state().1,
        Some(ProviderSettingsAction::StartSubscriptionLogin)
    );
    harness.state_mut().0.subscription_mut().unwrap().auth = SubscriptionAuthState::Exchanging;
    harness.run_steps(4);
    for label in ["Save", "Browser sign-in", "API key", "Cursor API key"] {
        assert!(
            harness.get_by_label(label).accesskit_node().is_disabled(),
            "{label}"
        );
    }
    harness.get_by_label("Cancel sign-in").click();
    harness.run_steps(4);
    assert_eq!(
        harness.state().1,
        Some(ProviderSettingsAction::CancelSubscriptionLogin)
    );
}

#[test]
fn claude_subscription_does_not_offer_cursor_api_key_authentication() {
    use egui_kittest::kittest::Queryable;
    let mut settings = ProviderSettingsModel::default();
    settings.add(ProviderKind::ClaudeSubscription);
    let mut harness = settings_harness(settings);
    harness.run_steps(8);
    assert!(harness.query_by_label("API key").is_none());
    assert!(harness.query_by_label("Cursor API key").is_none());
    assert!(harness.query_by_label("Sign in to Claude").is_some());
}
