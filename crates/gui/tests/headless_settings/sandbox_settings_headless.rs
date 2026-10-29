use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};

fn escalation_fixture(
    dir: &std::path::Path,
    initial: config::SandboxConfig,
) -> (HeadlessWorkbench<DemoSource>, runtime::AgentRuntime) {
    let path = config::project_main_config_path(dir);
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("config directory");
    config::save_sandbox(&path, initial).expect("save fixture");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        std::sync::Arc::new(tools::ToolExecutor::with_standard_tools(
            bus,
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        std::sync::Arc::new(runtime::compose::UnconfiguredModel),
    );
    runtime.set_web_tools_enabled(initial.web_tools_enabled);
    runtime.set_sandbox_escalation(
        initial.escalation_approval,
        initial.escalate_to_user_on_deny,
    );
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("state")
            .with_settings_load_options(config::LoadOptions {
                project_dir: Some(dir.to_path_buf()),
                user_config_dir: Some(dir.join(config::PROJECT_CONFIG_DIR)),
                read_env: false,
                ..Default::default()
            })
            .with_provider_settings_path(path)
            .with_sandbox_runtime(runtime.clone());
    state.open_sandbox_settings();
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    (harness, runtime)
}

fn reload_sandbox(dir: &std::path::Path) -> config::SandboxConfig {
    config::Config::load(&config::LoadOptions {
        project_dir: Some(dir.into()),
        user_config_dir: Some(dir.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .expect("reload")
    .sandbox
}

#[test]
fn chatbox_sandbox_button_opens_restored_modal() {
    // Given: defaults on disk and the real settings modal.
    let dir = tempfile::tempdir().expect("temp");
    let (mut harness, _) = escalation_fixture(dir.path(), config::SandboxConfig::default());
    harness.click_label("Cancel");
    harness.run();
    // When: opening settings from the chatbox.
    harness.click_label("Sandbox: auto review");
    harness.run();
    // Then: the modal exposes the sandbox settings, and Web tools live only in
    // Agent features so the two modals never disagree about them.
    assert!(!harness.has_label("Web tools"));
    for label in [
        "Ask me when auto review denies a request",
        "auto review",
        "ask me",
        "always deny",
        "yolo",
    ] {
        assert!(harness.has_label(label), "missing control: {label}");
    }
}

#[test]
fn settings_menu_no_longer_offers_sandbox() {
    // Given: the workbench with no modal open.
    let dir = tempfile::tempdir().expect("temp");
    let (mut harness, _) = escalation_fixture(dir.path(), config::SandboxConfig::default());
    harness.click_label("Cancel");
    harness.run();
    // When: opening the settings menu.
    harness.click_label("⚙");
    harness.run();
    // Then: sandbox settings have moved, while other settings remain available.
    assert!(!harness.has_label("Sandbox"));
    for label in [
        "Theme",
        "Providers",
        "Agent roles",
        "Agent features",
        "Routing",
        "Storage",
    ] {
        assert!(harness.has_label(label), "missing settings entry: {label}");
    }
}

#[test]
fn storage_settings_open_from_the_settings_menu_not_the_composer() {
    // Given: the workbench with no modal open.
    let dir = tempfile::tempdir().expect("temp");
    let (mut harness, _) = escalation_fixture(dir.path(), config::SandboxConfig::default());
    harness.click_label("Cancel");
    harness.run();
    // Then: the composer no longer carries a storage shortcut.
    assert!(!harness.has_label("Storage"));
    // When: choosing Storage from the settings menu.
    harness.click_label("⚙");
    harness.run();
    harness.click_label("Storage");
    harness.run();
    // Then: the storage modal explains its retention policy.
    assert!(harness.has_label("Conversation, task, and approval audit records are kept."));
}

#[test]
fn modal_escalation_change_applies_live_when_saved() {
    // Given: auto approval on disk and in the live runtime.
    let dir = tempfile::tempdir().expect("temp");
    let (mut harness, runtime) = escalation_fixture(dir.path(), config::SandboxConfig::default());
    // When: changing approval and deny escalation in the modal, then saving.
    harness.click_label("ask me");
    harness.run();
    harness.click_label("Ask me when auto review denies a request");
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    // Then: both values are persisted and applied to the existing runtime.
    let saved = reload_sandbox(dir.path());
    assert_eq!(saved.escalation_approval, config::EscalationApproval::User);
    assert!(saved.escalate_to_user_on_deny);
    let policy = runtime.execution_policy(runtime::Role::Worker);
    assert_eq!(policy.escalation_approval, config::EscalationApproval::User);
    assert!(policy.escalate_to_user_on_deny);
}

#[test]
fn modal_yolo_choice_is_persisted_and_applied_live_when_saved() {
    // Given: auto approval on disk and in the live runtime.
    let dir = tempfile::tempdir().expect("temp");
    let (mut harness, runtime) = escalation_fixture(dir.path(), config::SandboxConfig::default());
    // When: choosing yolo in the modal, then saving.
    harness.click_label("yolo");
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    // Then: the choice is persisted and applied to the existing runtime.
    assert_eq!(
        reload_sandbox(dir.path()).escalation_approval,
        config::EscalationApproval::Yolo
    );
    assert_eq!(
        runtime
            .execution_policy(runtime::Role::Worker)
            .escalation_approval,
        config::EscalationApproval::Yolo
    );
}

#[test]
fn sandbox_save_keeps_the_web_tools_setting_and_live_state() {
    // Given: Web tools disabled on disk and in the live runtime.
    let dir = tempfile::tempdir().expect("temp");
    let initial = config::SandboxConfig {
        web_tools_enabled: false,
        ..Default::default()
    };
    let (mut harness, runtime) = escalation_fixture(dir.path(), initial);
    // When: changing only the approval mode and saving the sandbox.
    harness.click_label("ask me");
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    // Then: Web tools stay disabled on disk and in the same runtime.
    assert_eq!(
        reload_sandbox(dir.path()),
        config::SandboxConfig {
            escalation_approval: config::EscalationApproval::User,
            ..initial
        }
    );
    assert!(!runtime.web_tools_enabled());
}

#[test]
#[ignore = "captures native GPU evidence"]
fn capture_sandbox_settings() {
    // Given: the Sandbox settings block at desktop geometry.
    let dir = tempfile::tempdir().expect("temp");
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("state")
            .with_settings_load_options(config::LoadOptions {
                project_dir: Some(dir.path().to_path_buf()),
                user_config_dir: Some(dir.path().join(config::PROJECT_CONFIG_DIR)),
                read_env: false,
                ..Default::default()
            })
            .with_provider_settings_path(config::project_main_config_path(dir.path()));
    state.open_sandbox_settings();
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // When: capturing the real egui rendering.
    let frame = harness.capture().expect("capture");
    frame
        .save_png(std::path::Path::new("/tmp/opencode/sandbox-settings.png"))
        .expect("PNG");
    // Then: controls fit within the viewport.
    for label in [
        "Ask me when auto review denies a request",
        "Save sandbox",
        "Cancel",
    ] {
        assert!(
            harness
                .label_rects(label)
                .iter()
                .all(|rect| harness.screen_rect().contains_rect(*rect))
        );
    }
}
