use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};

#[test]
fn sandbox_checkbox_persists_when_saved() {
    // Given: an existing project configuration and the settings menu.
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("evorch.toml");
    std::fs::write(&path, "version = 2\n[metrics]\nenabled = false\n").expect("fixture");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        std::sync::Arc::new(tools::ToolExecutor::with_standard_tools(
            bus,
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        std::sync::Arc::new(runtime::compose::UnconfiguredModel),
    );
    let state = WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
        .expect("state")
        .with_provider_settings_path(path.clone())
        .with_sandbox_runtime(runtime.clone());
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // When: enabling sandbox networking through the real checkbox and saving.
    harness.click_label("⚙");
    harness.run();
    harness.click_label("Sandbox");
    harness.run();
    harness.click_label("Allow network inside sandbox");
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    // Then: the typed setting is persisted without changing other sections.
    let saved = config::Config::load(&config::LoadOptions {
        project_dir: Some(dir.path().into()),
        user_config_dir: Some(dir.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("load");
    assert!(saved.sandbox.allow_network);
    assert!(!saved.metrics.enabled);
    assert_eq!(
        runtime
            .execution_policy(runtime::Role::Explorer)
            .sandbox_network_mode(),
        runtime::SandboxNetworkMode::ParentNetns
    );
    assert_eq!(
        runtime
            .execution_policy(runtime::Role::Worker)
            .sandbox_network_mode(),
        runtime::SandboxNetworkMode::Unshared
    );
}

#[test]
fn cancel_keeps_sandbox_setting_unchanged() {
    // Given: an enabled persisted setting.
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("evorch.toml");
    config::save_sandbox(
        &path,
        config::SandboxConfig {
            allow_network: true,
            ..Default::default()
        },
    )
    .expect("save");
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("state")
            .with_provider_settings_path(path.clone());
    state.open_sandbox_settings();
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // When: editing and cancelling instead of saving.
    harness.click_label("Allow network inside sandbox");
    harness.run();
    harness.click_label("Cancel");
    harness.run();
    // Then: reopening reloads the persisted enabled setting; saving preserves it.
    harness.state_mut().open_sandbox_settings();
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    let saved = config::Config::load(&config::LoadOptions {
        project_dir: Some(dir.path().into()),
        user_config_dir: Some(dir.path().join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("load");
    assert!(saved.sandbox.allow_network);
}

fn escalation_fixture(
    dir: &std::path::Path,
    initial: config::SandboxConfig,
) -> (HeadlessWorkbench<DemoSource>, runtime::AgentRuntime) {
    let path = dir.join("evorch.toml");
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
    runtime.set_sandbox_escalation(
        initial.escalation_approval,
        initial.escalate_to_user_on_deny,
    );
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("state")
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
        user_config_dir: Some(dir.join("user")),
        read_env: false,
        ..Default::default()
    })
    .expect("reload")
    .sandbox
}

#[test]
fn sandbox_settings_persist_escalation_approval_fields_when_saved() {
    // Given: defaults on disk and the real settings modal.
    let dir = tempfile::tempdir().expect("temp");
    let (mut harness, _) = escalation_fixture(dir.path(), config::SandboxConfig::default());
    // When: saving the fallback setting, then selecting modes in the composer.
    harness.click_label("審査で拒否された場合はユーザー承認へ昇格");
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    harness.click_label("Cancel");
    harness.run();
    let mut current = "quick";
    for (label, mode, serialized) in [
        ("user", config::EscalationApproval::User, "user"),
        ("off", config::EscalationApproval::Off, "off"),
        ("quick", config::EscalationApproval::Quick, "quick"),
    ] {
        harness.click_label(&format!("Sandbox: {current}"));
        harness.run();
        harness.click_label(label);
        harness.run();
        current = serialized;
        assert!(harness.has_label(&format!("Sandbox: {current}")));
        // Then: both explicit keys and typed values survive reloading.
        let saved = reload_sandbox(dir.path());
        assert_eq!(saved.escalation_approval, mode);
        assert!(saved.escalate_to_user_on_deny);
        let text = std::fs::read_to_string(dir.path().join("evorch.toml")).expect("read");
        assert!(
            text.lines()
                .any(|line| line == format!("escalation_approval = \"{serialized}\""))
        );
        assert!(
            text.lines()
                .any(|line| line == "escalate_to_user_on_deny = true")
        );
    }
}

#[test]
fn sandbox_settings_apply_live_updates_runtime_escalation_when_saved() {
    // Given: a runtime shared with the open modal, without recomposition.
    let dir = tempfile::tempdir().expect("temp");
    let (mut harness, runtime) = escalation_fixture(dir.path(), config::SandboxConfig::default());
    // When: applying a non-default approval mode and fallback.
    harness.click_label("審査で拒否された場合はユーザー承認へ昇格");
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    harness.click_label("Cancel");
    harness.run();
    harness.click_label("Sandbox: quick");
    harness.run();
    harness.click_label("user");
    harness.run();
    // Then: the same runtime exposes both updated settings immediately.
    let policy = runtime.execution_policy(runtime::Role::Worker);
    assert_eq!(policy.escalation_approval, config::EscalationApproval::User);
    assert!(policy.escalate_to_user_on_deny);
}

#[test]
fn sandbox_settings_cancel_keeps_escalation_unchanged_when_reopened() {
    // Given: non-default persisted settings, also installed in the runtime.
    let dir = tempfile::tempdir().expect("temp");
    let initial = config::SandboxConfig {
        escalation_approval: config::EscalationApproval::User,
        escalate_to_user_on_deny: true,
        ..Default::default()
    };
    let (mut harness, runtime) = escalation_fixture(dir.path(), initial);
    // When: editing the fallback, cancelling, then reopening and saving untouched.
    harness.click_label("審査で拒否された場合はユーザー承認へ昇格");
    harness.run();
    harness.click_label("Cancel");
    harness.run();
    // Then: cancellation changes neither the file nor the live runtime.
    assert_eq!(reload_sandbox(dir.path()), initial);
    let policy = runtime.execution_policy(runtime::Role::Worker);
    assert_eq!(policy.escalation_approval, initial.escalation_approval);
    assert!(policy.escalate_to_user_on_deny);
    harness.state_mut().open_sandbox_settings();
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    assert_eq!(reload_sandbox(dir.path()), initial);
}

#[test]
#[ignore = "captures native GPU evidence"]
fn capture_sandbox_settings() {
    // Given: the Sandbox settings block at desktop geometry.
    let dir = tempfile::tempdir().expect("temp");
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("state")
            .with_provider_settings_path(dir.path().join("evorch.toml"));
    state.open_sandbox_settings();
    let mut harness = HeadlessWorkbench::new(state, [960.0, 600.0]);
    harness.run();
    // When: capturing the real egui rendering.
    let frame = harness.capture().expect("capture");
    frame
        .save_png(std::path::Path::new("/tmp/opencode/sandbox-settings.png"))
        .expect("PNG");
    // Then: controls fit within the viewport.
    for label in ["Allow network inside sandbox", "Save sandbox", "Cancel"] {
        assert!(
            harness
                .label_rects(label)
                .iter()
                .all(|rect| harness.screen_rect().contains_rect(*rect))
        );
    }
}
