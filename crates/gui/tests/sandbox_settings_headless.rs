use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};

#[test]
fn sandbox_settings_network_toggle_persists_and_applies_live_when_saved() {
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
fn sandbox_settings_cancel_keeps_network_unchanged_when_reopened() {
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
fn sandbox_settings_modal_shows_network_toggle_without_escalation_checkbox() {
    // Given: defaults on disk and the real settings modal.
    let dir = tempfile::tempdir().expect("temp");
    let (mut harness, _) = escalation_fixture(dir.path(), config::SandboxConfig::default());
    // When: closing and reopening the modal.
    harness.click_label("Cancel");
    harness.run();
    assert!(!harness.has_label("Allow network inside sandbox"));
    harness.state_mut().open_sandbox_settings();
    harness.run();
    // Then: only the network setting is exposed by the modal.
    assert!(harness.has_label("Allow network inside sandbox"));
    for label in [
        "審査で拒否された場合はユーザー承認へ昇格",
        "escalation",
        "Escalation",
        "承認",
    ] {
        assert_eq!(harness.count_labels(label), 0, "unexpected label: {label}");
    }
}

#[test]
fn sandbox_settings_save_preserves_persisted_escalation_fields() {
    // Given: non-default persisted settings, also installed in the runtime.
    let dir = tempfile::tempdir().expect("temp");
    let initial = config::SandboxConfig {
        escalation_approval: config::EscalationApproval::User,
        escalate_to_user_on_deny: true,
        ..Default::default()
    };
    let (mut harness, runtime) = escalation_fixture(dir.path(), initial);
    // When: changing only networking and saving the loaded configuration.
    harness.click_label("Allow network inside sandbox");
    harness.run();
    harness.click_label("Save sandbox");
    harness.run();
    // Then: disk and the same live runtime retain both non-default approval fields.
    assert_eq!(
        reload_sandbox(dir.path()),
        config::SandboxConfig {
            allow_network: true,
            ..initial
        }
    );
    let policy = runtime.execution_policy(runtime::Role::Worker);
    assert_eq!(policy.escalation_approval, initial.escalation_approval);
    assert!(policy.escalate_to_user_on_deny);
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
