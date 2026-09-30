use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use workspace_ui::{PanelId, UiSettings};

fn fixture(path: &std::path::Path) -> HeadlessWorkbench<DemoSource> {
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_provider_settings_path(path.join("evorch.toml"));
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn reload(path: &std::path::Path) -> config::Config {
    config::Config::load(&config::LoadOptions {
        project_dir: Some(path.into()),
        user_config_dir: Some(path.join("user")),
        read_env: false,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn composer_settings_entry_shows_defaults_and_saves_for_restart_only() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("evorch.toml"),
        "version = 2\n[metrics]\nenabled = false\n",
    )
    .unwrap();
    let mut harness = fixture(dir.path());
    harness.run();
    harness.click_label("Self-improvement settings");
    harness.run();
    for label in [
        "Enable self-improvement drafts (default off)",
        "Current session: disabled (default off)",
        "Drafts only — evorch never files issues automatically (ADR 0011).",
        "Collect diagnostics",
        "Collect lessons",
        "Draft directory",
        "Max candidates",
        "Evidence max bytes",
        "Daily limit",
        "Duplicate cooldown (seconds)",
        "default: 200",
        "default: 2048",
        "default: 20",
        "default: 86400",
    ] {
        assert!(harness.has_label(label), "missing {label}");
    }
    harness.click_label("Enable self-improvement drafts (default off)");
    harness.click_label("Collect lessons");
    harness.click_label("Save self-improvement");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        harness.run();
        if reload(dir.path()).self_improvement.enabled {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "save timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let saved = reload(dir.path());
    assert!(!saved.self_improvement.collect_lessons);
    assert!(!saved.metrics.enabled);
    assert!(harness.has_label("Current session: disabled (default off)"));
    assert!(!dir.path().join("self-improvement").exists());
}

#[test]
fn cancel_discards_toggle_and_other_settings_close_the_modal() {
    let dir = tempfile::tempdir().unwrap();
    config::save_self_improvement(&dir.path().join("evorch.toml"), &Default::default()).unwrap();
    let mut harness = fixture(dir.path());
    harness.state_mut().open_self_improvement_settings();
    harness.run();
    harness.click_label("Enable self-improvement drafts (default off)");
    harness.click_label("Cancel");
    harness.run();
    assert!(!reload(dir.path()).self_improvement.enabled);
    harness.state_mut().open_self_improvement_settings();
    harness.run();
    harness.state_mut().open_sandbox_settings();
    harness.run();
    assert!(!harness.has_label("Enable self-improvement drafts (default off)"));
    assert!(harness.has_label("Enable web tools"));
}

#[test]
fn companion_tab_is_registered_once_and_renders_disabled_without_storage_access() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("not-created.db"),
        ..Default::default()
    };
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_memory_storage(config.clone())
        .with_memory_storage(config.clone());
    let id = PanelId::new("self-improvement-main");
    assert_eq!(
        state
            .dock()
            .iter_all_tabs()
            .filter(|(_, tab)| *tab == &id)
            .count(),
        1
    );
    let path = state.dock().find_tab(&id).unwrap();
    state.dock_mut().set_active_tab(path).unwrap();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    assert!(harness.has_label("Self-improvement drafts are disabled ([self_improvement] enabled=false). Candidates are not collected."));
    assert!(!config.db_path.exists());
}
