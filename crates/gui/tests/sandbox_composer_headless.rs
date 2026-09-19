use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};

fn workbench(
    root: &std::path::Path,
    mode: config::EscalationApproval,
) -> HeadlessWorkbench<DemoSource> {
    let path = root.join("evorch.toml");
    config::save_sandbox(
        &path,
        config::SandboxConfig {
            escalation_approval: mode,
            ..Default::default()
        },
    )
    .expect("fixture");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
        .expect("state")
        .with_provider_settings_path(path);
    HeadlessWorkbench::new(state, [960.0, 600.0])
}

#[test]
fn composer_exposes_sandbox_left_and_model_right_when_rendered() {
    // Given: a closed settings modal and the composer.
    let dir = tempfile::tempdir().expect("temp");
    let mut harness = workbench(dir.path(), config::EscalationApproval::Auto);
    // When: rendering the app.
    harness.run();
    // Then: both selectors occupy the same row, above the input, at opposite edges.
    assert!(harness.has_label("Sandbox: auto"));
    let sandbox = harness.label_rects("Sandbox: auto")[0];
    let model = harness.label_rects("Select model")[0];
    let input = harness.label_rects("Message or /command")[0];
    assert!(
        (sandbox.center().y - model.center().y).abs() < 1.0,
        "sandbox={sandbox:?}, model={model:?}"
    );
    assert!(sandbox.right() < model.left());
    assert!(sandbox.bottom() < input.top());
    assert!((sandbox.left() - input.left()).abs() < 20.0);
    let send = harness.label_rects("Send")[0];
    assert!((model.right() - send.right()).abs() < 1.0);
}

#[test]
fn composer_loads_persisted_mode_when_modal_was_never_opened() {
    // Given: a non-default persisted mode.
    let dir = tempfile::tempdir().expect("temp");
    let mut harness = workbench(dir.path(), config::EscalationApproval::Off);
    // When: starting the workbench without opening settings.
    harness.run();
    // Then: the composer reflects the persisted mode.
    assert!(harness.has_label("Sandbox: off"));
}

#[test]
fn modal_keeps_checkboxes_without_escalation_radios_when_opened() {
    // Given: a configured workbench.
    let dir = tempfile::tempdir().expect("temp");
    let mut harness = workbench(dir.path(), config::EscalationApproval::Auto);
    // When: opening the sandbox modal.
    harness.state_mut().open_sandbox_settings();
    harness.run();
    // Then: networking and deny fallback remain, but mode selection is absent.
    assert!(harness.has_label("Allow network inside sandbox"));
    assert!(harness.has_label("審査で拒否された場合はユーザー承認へ昇格"));
    for label in [
        "エスカレーション審査",
        "auto モデル審査 (既定)",
        "ユーザー承認",
        "無効",
    ] {
        assert!(
            !harness.has_label(label),
            "unexpected modal control: {label}"
        );
    }
}

#[test]
fn composer_reports_save_failure_without_changing_mode_when_path_is_unwritable() {
    // Given: the config path becomes a directory after startup.
    let dir = tempfile::tempdir().expect("temp");
    let mut harness = workbench(dir.path(), config::EscalationApproval::Auto);
    harness.run();
    let path = dir.path().join("evorch.toml");
    std::fs::remove_file(&path).expect("remove fixture");
    std::fs::create_dir(&path).expect("block config path");
    // When: selecting a mode that cannot be persisted.
    harness.click_label("Sandbox: auto");
    harness.run();
    harness.click_label("user");
    harness.run();
    // Then: the existing error surface opens and the selection is rolled back.
    assert!(harness.has_label("Save sandbox"));
    harness.click_label("Cancel");
    harness.run();
    assert!(harness.has_label("Sandbox: auto"));
}

#[test]
#[ignore = "captures native GPU evidence"]
fn capture_sandbox_composer_states() {
    // Given: the real composer at two desktop viewport sizes.
    let dir = tempfile::tempdir().expect("temp");
    for size in [[960.0, 600.0], [1280.0, 800.0]] {
        let mut harness = workbench(dir.path(), config::EscalationApproval::Auto);
        harness.input_mut().screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(size[0], size[1]),
        ));
        harness.run();
        // When: rendering closed, expanded and selected states.
        for state in ["closed", "expanded", "selected"] {
            match state {
                "expanded" => harness.click_label("Sandbox: auto"),
                "selected" => harness.click_label("user"),
                _ => {}
            }
            harness.run();
            // Then: capture the actual egui pixels for visual inspection.
            harness
                .capture()
                .expect("capture")
                .save_png(std::path::Path::new(&format!(
                    "/tmp/opencode/sandbox-composer-{}-{state}.png",
                    size[0]
                )))
                .expect("PNG");
        }
    }
}
