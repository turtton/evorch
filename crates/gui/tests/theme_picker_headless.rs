use gui::theme::style::ThemePreset;
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use workspace_ui::{ThemePresetName, UiSettings};

fn isolated(name: &str) -> bool {
    const CHILD: &str = "EVORCH_THEME_PICKER_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success());
    true
}

#[test]
fn state_restores_each_persisted_preset() {
    // Given: each framework-independent persisted preset.
    for (theme_preset, expected) in [
        (ThemePresetName::Graphite, ThemePreset::Graphite),
        (ThemePresetName::TokyoNight, ThemePreset::TokyoNight),
        (ThemePresetName::HighContrast, ThemePreset::HighContrast),
    ] {
        let settings = UiSettings {
            theme_preset,
            ..UiSettings::default()
        };
        // When: the workbench is constructed, without installing a global palette.
        let state = WorkbenchState::new(DemoSource(Vec::new()), &settings).unwrap();
        // Then: its first-frame preset matches the saved value.
        assert_eq!(state.theme_preset(), expected);
    }
}

fn picker(path: &std::path::Path) -> HeadlessWorkbench<DemoSource> {
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_ui_settings_path(path);
    let mut workbench = HeadlessWorkbench::new(state, [1280.0, 720.0]);
    workbench.run();
    workbench.click_label("Workbench settings");
    workbench.run();
    workbench.click_label("Theme");
    workbench.run();
    workbench
}

#[test]
fn theme_rows_select_tokyo_night_live() {
    if isolated("theme_rows_select_tokyo_night_live") {
        return;
    }
    // Given: the theme picker opened through the workbench menu.
    let dir = tempfile::tempdir().unwrap();
    let mut workbench = picker(&dir.path().join("ui.toml"));
    for label in ["Graphite", "Tokyo Night", "High Contrast"] {
        assert!(workbench.has_label(label), "missing {label}");
    }
    // When: Tokyo Night is selected by its accessible label.
    workbench.click_label("Tokyo Night");
    workbench.run();
    // Then: state and installed palette change without a restart.
    assert_eq!(workbench.state().theme_preset(), ThemePreset::TokyoNight);
    assert_eq!(
        gui::theme::tokens::palette(),
        ThemePreset::TokyoNight.palette()
    );
    workbench.click_label("Close");
    workbench.run();
    assert!(!workbench.has_label("Tokyo Night"));
}

#[test]
fn selection_persists_and_reloads_tokyo_night() {
    if isolated("selection_persists_and_reloads_tokyo_night") {
        return;
    }
    // Given: an isolated settings file, not the user's configuration.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ui.toml");
    let mut workbench = picker(&path);
    // When: the user selects a preset, without any separate save action.
    workbench.click_label("Tokyo Night");
    workbench.run();
    // Then: TOML and a fresh workbench both restore that preset.
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("theme_preset = \"tokyo_night\"")
    );
    let settings = workspace_ui::load_settings(&path).unwrap();
    assert_eq!(settings.theme_preset, ThemePresetName::TokyoNight);
    let restored = WorkbenchState::new(DemoSource(Vec::new()), &settings).unwrap();
    assert_eq!(restored.theme_preset(), ThemePreset::TokyoNight);
}

#[test]
fn save_failure_keeps_live_theme_and_reports_error() {
    if isolated("save_failure_keeps_live_theme_and_reports_error") {
        return;
    }
    // Given: a directory where a settings file must be written.
    let dir = tempfile::tempdir().unwrap();
    let mut workbench = picker(dir.path());
    // When: a theme is selected but the write fails.
    workbench.click_label("Tokyo Night");
    workbench.run();
    // Then: live selection remains usable and the I/O failure is visible.
    assert_eq!(workbench.state().theme_preset(), ThemePreset::TokyoNight);
    let expected = workspace_ui::save_settings(&UiSettings::default(), dir.path()).unwrap_err();
    assert!(workbench.has_label(&expected.to_string()));
}
