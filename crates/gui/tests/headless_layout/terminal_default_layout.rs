use gui::{app::WorkbenchState, fixture::DemoSource};
use workspace_ui::{PanelId, UiSettings};

#[test]
fn terminal_is_collapsed_when_workspace_is_fresh() {
    // Given: no persisted workspace.
    let settings = UiSettings::default();
    // When: the workbench creates its default dock.
    let state = WorkbenchState::new(DemoSource(Vec::new()), &settings).unwrap();
    // Then: the terminal leaf starts minimized.
    let path = state
        .dock()
        .find_tab(&PanelId::new("terminal-main"))
        .unwrap();
    assert!(state.dock().leaf(path.node_path()).unwrap().collapsed);
}

#[test]
fn terminal_stays_expanded_when_workspace_is_explicit() {
    // Given: an explicitly saved expanded workspace.
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(Default::default());
    // When: that workspace is restored.
    let state = WorkbenchState::new(DemoSource(Vec::new()), &settings).unwrap();
    // Then: the user's expanded state wins.
    let path = state
        .dock()
        .find_tab(&PanelId::new("terminal-main"))
        .unwrap();
    assert!(!state.dock().leaf(path.node_path()).unwrap().collapsed);
}
