use workspace_ui::{ProjectId, SidebarState};

fn projects() -> (tempfile::TempDir, SidebarState) {
    let root = tempfile::tempdir().expect("fixture");
    let mut sidebar = SidebarState::default();
    for name in ["first", "second"] {
        let path = root.path().join(name);
        std::fs::create_dir(&path).expect("project directory");
        sidebar
            .add_project(ProjectId::new(name), name, &path)
            .expect("project");
    }
    (root, sidebar)
}

#[test]
fn explicit_primary_wins_over_selected() {
    // Given: two projects with different selected and primary IDs.
    let (_root, mut sidebar) = projects();
    sidebar
        .select_project(&ProjectId::new("first"))
        .expect("selection");
    sidebar
        .set_primary_project(Some(ProjectId::new("second")))
        .expect("primary");
    // When: resolving the shell project.
    let resolved = sidebar.resolved_primary_project();
    // Then: primary wins.
    assert_eq!(
        resolved.map(|project| &project.id),
        Some(&ProjectId::new("second"))
    );
}

#[test]
fn single_project_wins_without_selection() {
    // Given: one registered project and a stale primary.
    let (_root, mut sidebar) = projects();
    sidebar.projects.pop();
    sidebar.primary_project = Some(ProjectId::new("removed"));
    // When / Then: a stale primary does not hide the only project.
    assert_eq!(
        sidebar
            .resolved_primary_project()
            .map(|project| &project.id),
        Some(&ProjectId::new("first"))
    );
}

#[test]
fn selected_project_is_used_after_primary_is_cleared() {
    // Given: two projects and an explicit primary.
    let (_root, mut sidebar) = projects();
    sidebar
        .select_project(&ProjectId::new("second"))
        .expect("selection");
    sidebar
        .set_primary_project(Some(ProjectId::new("first")))
        .expect("primary");
    // When: the primary is cleared.
    sidebar.set_primary_project(None).expect("clear primary");
    // Then: selection resolves the ambiguity.
    assert_eq!(
        sidebar
            .resolved_primary_project()
            .map(|project| &project.id),
        Some(&ProjectId::new("second"))
    );
}

#[test]
fn stale_primary_falls_back_to_valid_selection() {
    // Given: a removed primary and a valid selected project.
    let (_root, mut sidebar) = projects();
    sidebar.primary_project = Some(ProjectId::new("removed"));
    sidebar
        .select_project(&ProjectId::new("second"))
        .expect("selection");
    // When / Then: resolution ignores the removed project.
    assert_eq!(
        sidebar
            .resolved_primary_project()
            .map(|project| &project.id),
        Some(&ProjectId::new("second"))
    );
}

#[test]
fn ambiguous_projects_without_valid_selection_remain_unresolved() {
    // Given: multiple projects and a stale selection.
    let (_root, mut sidebar) = projects();
    sidebar.selected_project = Some(ProjectId::new("removed"));
    // When / Then: no arbitrary first project is selected.
    assert_eq!(sidebar.resolved_primary_project(), None);
}

#[test]
fn primary_survives_sidebar_persistence() {
    // Given: a primary project differing from selection.
    let (root, mut sidebar) = projects();
    sidebar
        .select_project(&ProjectId::new("first"))
        .expect("selection");
    sidebar
        .set_primary_project(Some(ProjectId::new("second")))
        .expect("primary");
    let path = root.path().join("sidebar.json");
    // When: saving and loading through the existing persistence API.
    workspace_ui::save_sidebar(&sidebar, &path).expect("save");
    let loaded = workspace_ui::load_sidebar(&path).expect("load");
    // Then: both explicit state and its effective project survive.
    assert_eq!(loaded, sidebar);
    assert_eq!(
        loaded.resolved_primary_project().map(|project| &project.id),
        Some(&ProjectId::new("second"))
    );
}

#[test]
fn legacy_json_loads_without_primary_field() {
    // Given: the old JSON schema, with no primary field.
    let json =
        r#"{"version":1,"projects":[],"selected_project":null,"threads":[],"active_thread":null}"#;
    // When: loaded using existing persistence.
    let sidebar = workspace_ui::sidebar_from_json(json).expect("legacy sidebar");
    // Then: the added optional state defaults to absent.
    assert_eq!(sidebar.primary_project, None);
    assert_eq!(sidebar.resolved_primary_project(), None);
}
