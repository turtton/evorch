use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use workspace_ui::{PanelId, UiSettings, Workspace};

#[test]
fn approvals_tab_is_not_registered_on_creation_reset_or_reload() {
    for mut workspace in [Workspace::default_v01(), Workspace::default_v02()] {
        workspace.version = workspace_ui::WORKSPACE_SCHEMA_VERSION;
        let mut settings = UiSettings::default();
        settings.layout.workspace = Some(workspace);
        let state = WorkbenchState::new(DemoSource(vec![]), &settings).unwrap();
        let mut ui = HeadlessWorkbench::new(state, [1280.0, 900.0]);
        ui.run();
        assert!(
            ui.state()
                .dock()
                .find_tab(&PanelId::new("approvals-main"))
                .is_none()
        );
        ui.key_press(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::R,
        );
        ui.run();
        assert!(
            ui.state()
                .dock()
                .find_tab(&PanelId::new("approvals-main"))
                .is_none()
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let saved =
            gui::dock::from_dock_state(ui.state().dock(), &Workspace::default().panels).unwrap();
        workspace_ui::save_to(&saved, &path).unwrap();
        settings.layout.workspace = Some(workspace_ui::load_from(&path).unwrap());
        let restored = WorkbenchState::new(DemoSource(vec![]), &settings).unwrap();
        assert!(
            restored
                .dock()
                .find_tab(&PanelId::new("approvals-main"))
                .is_none()
        );
    }
}

#[test]
fn retired_approval_panel_is_not_a_supported_persisted_kind() {
    // Internal v0.x layouts deliberately do not preserve removed panel kinds.
    assert!(serde_json::from_str::<workspace_ui::PanelKind>("\"approvals\"").is_err());
}
