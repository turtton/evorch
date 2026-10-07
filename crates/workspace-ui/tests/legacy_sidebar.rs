// Sidebars saved while a primary project existed still load; the field is ignored.
#[test]
fn sidebar_saved_with_a_primary_project_still_loads() {
    let json = r#"{"version":1,"projects":[],"selected_project":null,"primary_project":"gone","threads":[],"active_thread":null}"#;

    let sidebar = workspace_ui::sidebar_from_json(json).expect("legacy sidebar");

    assert_eq!(sidebar, workspace_ui::SidebarState::default());
}
