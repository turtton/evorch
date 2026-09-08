use config::{ApiProtocolConfig, CredentialRefConfig, ProviderTypeConfig};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::codex_auth::CODEX_LOGIN_BUTTON;
use gui::model::commands::{ChatSubmission, WorkbenchCommand};
use gui::model::composer::{GOAL_PROVIDER_GUIDANCE, PROVIDER_MISSING_GUIDANCE, ProviderStatus};
use gui::model::provider_settings::ProviderSettingsModel;
use gui::theme::tokens::PROVIDER_MODAL_MAX_WIDTH;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

#[path = "provider_settings/fetch.rs"]
mod fetch;

#[path = "provider_settings/tabs.rs"]
mod tabs;

#[path = "provider_settings/capture.rs"]
mod capture;

#[path = "provider_settings/codex_auth.rs"]
mod codex_auth;

fn workbench(root: &std::path::Path, provider: ProviderStatus) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project added");
    sidebar
        .select_project(&project_id)
        .expect("project selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread selected");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
        .with_provider_status(provider);
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

#[test]
fn open_settings_button_visible_when_not_configured() {
    // Given: conversations with and without a configured provider.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut missing = workbench(temp.path(), ProviderStatus::default());
    let mut configured = workbench(temp.path(), ProviderStatus::Configured);
    // When: both conversations render.
    missing.run();
    configured.run();
    // Then: only the unconfigured conversation offers settings.
    assert!(missing.has_label(PROVIDER_MISSING_GUIDANCE));
    assert!(missing.has_label("Open Settings"));
    assert!(!configured.has_label("Open Settings"));
}

#[test]
fn clicking_open_settings_shows_modal_fields() {
    // Given: an unconfigured conversation.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    harness.run();
    // When: settings are opened through the composer.
    harness.click_label("Open Settings");
    harness.run();
    // Then: the settings modal offers save and cancel.
    assert!(harness.has_label("Provider settings"));
    assert!(harness.has_label("Save"));
    assert!(harness.has_label("Cancel"));
}

#[test]
fn settings_button_opens_modal_when_provider_is_configured() {
    // Given: a configured conversation offers the compact settings action.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::Configured);
    harness.run();
    assert!(!harness.has_label("Open Settings"));
    assert!(harness.has_label("Settings"));
    // When: settings are opened through the composer.
    harness.click_label("Settings");
    harness.run();
    // Then: the modal still offers Codex login.
    assert!(harness.has_label("Provider settings"));
    assert!(harness.has_label(CODEX_LOGIN_BUTTON));
}

fn workbench_with_config_path(root: &std::path::Path) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project added");
    sidebar
        .select_project(&project_id)
        .expect("project selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread selected");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::default())
        .with_provider_settings_path(root.join("evorch.toml"));
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn open_valid_settings(harness: &mut HeadlessWorkbench<DemoSource>) {
    harness.run();
    harness.click_label("Open Settings");
    harness.run();
    let model = harness.state_mut().provider_settings_mut();
    model.name = "local".into();
    model.base_url = "https://api.example.invalid/v1".into();
    model.api_key_env = "LOCAL_API_KEY".into();
    model.models_text = "gpt-4.1\ngpt-4.1-mini".into();
    model.default_model = "gpt-4.1".into();
    harness.run();
}

fn load_config(root: &std::path::Path) -> config::Config {
    config::Config::load(&config::LoadOptions {
        project_dir: Some(root.to_path_buf()),
        user_config_dir: Some(root.join("isolated-user-config")),
        read_env: false,
        ..Default::default()
    })
    .expect("saved config loads")
}

fn activate_goal(harness: &mut HeadlessWorkbench<DemoSource>) {
    let dock = harness.state_mut().dock_mut();
    let path = dock
        .find_tab(&PanelId::new("goal-main"))
        .expect("goal tab exists");
    dock.leaf_mut(path.node_path())
        .expect("leaf exists")
        .set_active_tab(path.tab.0)
        .expect("tab index is valid");
    harness.run();
}

#[test]
fn save_valid_settings_writes_evorch_toml_and_flips_status() {
    // Given: an unconfigured conversation and valid settings with a project path.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_config_path(temp.path());
    open_valid_settings(&mut harness);
    // When: settings are saved through the modal.
    harness.click_label("Save");
    harness.run();
    // Then: disk configuration and the conversation both become configured.
    let path = temp.path().join("evorch.toml");
    assert!(path.exists());
    let raw = std::fs::read_to_string(path).expect("saved file readable");
    assert!(raw.contains("type = \"openai-compatible\""));
    assert!(raw.contains("api_key_env = \"LOCAL_API_KEY\""));
    let config = load_config(temp.path());
    let provider = config.providers.get("local").expect("local provider saved");
    assert_eq!(provider.provider_type, ProviderTypeConfig::OpenAiCompatible);
    assert_eq!(
        provider.credential,
        CredentialRefConfig::Env {
            var: "LOCAL_API_KEY".into()
        }
    );
    assert_eq!(provider.api_protocol, ApiProtocolConfig::OpenAiCompletions);
    assert_eq!(provider.base_url, "https://api.example.invalid/v1");
    assert_eq!(provider.models, ["gpt-4.1", "gpt-4.1-mini"]);
    assert_eq!(provider.default_model, "gpt-4.1");
    assert_eq!(
        harness.state().provider_status(),
        &ProviderStatus::Configured
    );
    assert!(!harness.has_label(PROVIDER_MISSING_GUIDANCE));
    harness.state_mut().composer_mut().input = "hello".into();
    harness.run();
    harness.click_label("Send");
    harness.run();
    assert_eq!(
        harness.state().issued(),
        &[WorkbenchCommand::SendChat(ChatSubmission {
            thread_id: "thread-1".into(),
            text: "hello".into(),
        })]
    );
}

#[test]
fn save_with_plaintext_api_key_is_rejected_and_file_untouched() {
    // Given: otherwise valid settings contain a plaintext key instead of an env name.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_config_path(temp.path());
    open_valid_settings(&mut harness);
    harness.state_mut().provider_settings_mut().api_key_env = "sk-live-abc".into();
    harness.run();
    // When: saving is attempted.
    harness.click_label("Save");
    harness.run();
    // Then: no file or status change occurs and the rejection is visible in the modal.
    assert!(!temp.path().join("evorch.toml").exists());
    assert_eq!(
        harness.state().provider_status(),
        &ProviderStatus::default()
    );
    assert!(harness.has_label("Save"));
    let error = harness
        .state()
        .provider_settings()
        .error
        .as_deref()
        .expect("inline error");
    assert!(error.contains("api_key_env"), "{error}");
    assert!(error.contains("plaintext"), "{error}");
    assert!(harness.has_label(error));
}

#[test]
fn save_preserves_existing_tables_in_evorch_toml() {
    // Given: a canonical provider and unrelated routing configuration already exist.
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("evorch.toml");
    let existing = "version = 2\n# keep this comment\n[routing]\nroutes = {}\n\n[providers.other]\nprovider_type = \"anthropic\"\napi_protocol = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = { type = \"env\", var = \"OTHER_API_KEY\" }\nmodels = [\"claude-sonnet-4-5\"]\ndefault_model = \"claude-sonnet-4-5\"\n";
    std::fs::write(&path, existing).expect("existing config written");
    let before = load_config(temp.path());
    let mut harness = workbench_with_config_path(temp.path());
    open_valid_settings(&mut harness);
    // When: a different provider is added through Settings.
    harness.click_label("Save");
    harness.run();
    // Then: the existing table values and comment survive alongside the new provider.
    let raw = std::fs::read_to_string(path).expect("saved file readable");
    for line in existing.lines() {
        assert!(
            raw.lines().any(|saved| saved == line),
            "missing preserved line: {line}"
        );
    }
    let config = load_config(temp.path());
    assert_eq!(config.providers.len(), 2);
    assert!(config.providers.contains_key("local"));
    assert_eq!(config.providers.get("other"), before.providers.get("other"));
}

#[test]
fn cancel_closes_modal_without_writing() {
    // Given: a populated modal with a writable project path.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_config_path(temp.path());
    open_valid_settings(&mut harness);
    // When: the user cancels rather than saving.
    harness.click_label("Cancel");
    harness.run();
    // Then: the modal closes without configuring the provider or writing a file.
    assert!(!harness.has_label("Save"));
    assert!(!harness.state().provider_settings().open);
    assert!(!temp.path().join("evorch.toml").exists());
    assert_eq!(
        harness.state().provider_status(),
        &ProviderStatus::default()
    );
}

#[test]
fn goal_pane_shows_guidance_and_configure_button_when_not_configured() {
    // Given: the Goal pane is active without a provider.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    activate_goal(&mut harness);
    assert!(harness.has_label(GOAL_PROVIDER_GUIDANCE));
    assert!(harness.has_label("Configure provider"));
    harness.state_mut().goal_form_mut().goal = "x".into();
    harness.run();
    // When: the user submits a goal despite the guidance.
    harness.click_label("Submit");
    harness.run();
    // Then: provider setup is not a prerequisite for goal submission.
    let [WorkbenchCommand::SubmitGoal(goal)] = harness.state().issued() else {
        panic!(
            "expected exactly one SubmitGoal, got {:?}",
            harness.state().issued()
        );
    };
    assert_eq!(goal.goal, "x");
}

#[test]
fn configure_provider_button_in_goal_pane_opens_modal() {
    // Given: the unconfigured Goal pane offers provider setup.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    activate_goal(&mut harness);
    // When: the Goal pane's configure action is clicked.
    harness.click_label("Configure provider");
    harness.run();
    // Then: the same provider Settings modal opens.
    assert!(harness.has_label("Provider settings"));
    assert!(harness.has_label("Save"));
}

fn workbench_with_seeded_settings(
    root: &std::path::Path,
    model: ProviderSettingsModel,
    size: [f32; 2],
) -> HeadlessWorkbench<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project added");
    sidebar
        .select_project(&project_id)
        .expect("project selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread selected");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::default())
        .with_provider_settings_path(root.join("evorch.toml"))
        .with_provider_settings(model);
    HeadlessWorkbench::new(state, size)
}

#[test]
fn save_without_config_path_shows_inline_error() {
    // Given: valid settings but no configured save path.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench(temp.path(), ProviderStatus::default());
    open_valid_settings(&mut harness);
    // When: the user attempts to save.
    harness.click_label("Save");
    harness.run();
    // Then: the modal remains open with a visible path error and unchanged status.
    assert!(harness.has_label("Save"));
    let error = harness
        .state()
        .provider_settings()
        .error
        .as_deref()
        .expect("inline error");
    assert!(error.contains("config path"), "{error}");
    assert!(harness.has_label(error));
    assert_eq!(
        harness.state().provider_status(),
        &ProviderStatus::default()
    );
}

#[test]
fn modal_width_scales_with_viewport_and_respects_cap() {
    // Given: a provider with a very long base URL and model name.
    let temp = tempfile::tempdir().expect("temp dir");
    let model = ProviderSettingsModel {
        name: "local".into(),
        base_url: "https://api.example.invalid/v1/chat/completions/very/long/path/that/should/not/clip/in/the/provider/settings/modal".into(),
        api_key_env: "LOCAL_API_KEY_WITH_A_VERY_LONG_NAME".into(),
        models_text: "org/example/model-name-that-is-very-long-and-should-not-clip".into(),
        default_model: "org/example/model-name-that-is-very-long-and-should-not-clip".into(),
        ..ProviderSettingsModel::default()
    };
    for (viewport_width, expect_capped) in [(1200.0, true), (800.0, false)] {
        let mut harness =
            workbench_with_seeded_settings(temp.path(), model.clone(), [viewport_width, 600.0]);
        // When: settings are opened at the given viewport width.
        harness.click_label("Open Settings");
        harness.run();
        // Then: controls are visible and the modal fits within the viewport.
        let caption =
            "Used when a route doesn't override the model and when re-resolving a pinned session.";
        let caption_rects = harness.label_rects(caption);
        assert!(
            !caption_rects.is_empty(),
            "Default model caption should exist at {viewport_width}px"
        );
        let caption_rect = caption_rects[0];
        assert!(
            caption_rect.right() <= viewport_width,
            "Default model caption right edge ({}) must fit within {viewport_width}px viewport",
            caption_rect.right()
        );

        let base_url_rects = harness.label_rects("Base URL");
        assert!(
            !base_url_rects.is_empty(),
            "Base URL input should exist at {viewport_width}px"
        );
        let input_rect = base_url_rects[0];
        if expect_capped {
            assert!(
                input_rect.width() > 400.0,
                "input field should be wider than default narrow width, got {}",
                input_rect.width()
            );
        }

        let name_rects = harness.label_rects("Name");
        assert!(
            !name_rects.is_empty(),
            "Name label should exist at {viewport_width}px"
        );
        let name_rect = name_rects[0];
        // Modal width ≈ rightmost input right - leftmost label left + frame margins.
        let modal_width_approx = input_rect.right() - name_rect.left() + 50.0;
        assert!(
            modal_width_approx <= PROVIDER_MODAL_MAX_WIDTH,
            "modal should not exceed PROVIDER_MODAL_MAX_WIDTH ({PROVIDER_MODAL_MAX_WIDTH}), got {modal_width_approx}"
        );
    }
}
