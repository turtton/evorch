use tempfile::tempdir;
use workspace_ui::{
    AllowedDirectory, ProjectId, ProjectRecord, SidebarError, SidebarState, ThreadId, ThreadRecord,
    TrustState, load_sidebar, save_sidebar, sidebar_from_json, sidebar_to_json,
};

fn valid_sidebar() -> SidebarState {
    let directory = tempdir().expect("temporary directory must be created");
    let root = directory.keep().join("project");
    std::fs::create_dir(&root).expect("project directory must be created");
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("p1");
    sidebar
        .add_project(project_id.clone(), "One", &root)
        .expect("project must be added");
    sidebar
        .create_thread(ThreadId::new("t1"), project_id, "Thread")
        .expect("thread must be added");
    sidebar
}

#[test]
fn sidebar_json_roundtrip() {
    // Given: a valid sidebar and an isolated persistence path.
    let sidebar = valid_sidebar();
    let directory = tempdir().expect("temporary directory must be created");
    let path = directory.path().join("sidebar.json");

    // When: both JSON and file persistence surfaces round-trip it.
    let json = sidebar_to_json(&sidebar).expect("sidebar must serialize");
    let memory = sidebar_from_json(&json).expect("sidebar must deserialize");
    save_sidebar(&sidebar, &path).expect("sidebar must save");
    let file = load_sidebar(&path).expect("sidebar must load");

    // Then: both restore the complete model.
    assert_eq!(memory, sidebar);
    assert_eq!(file, sidebar);
}

#[test]
fn load_rejects_unknown_project_ref_duplicate_thread_and_invalid_trust_path() {
    // Given: independently malformed sidebar states.
    let valid = valid_sidebar();
    let mut unknown = valid.clone();
    unknown.threads[0].project_id = ProjectId::new("missing");
    let mut duplicate = valid.clone();
    duplicate.threads.push(duplicate.threads[0].clone());
    let mut relative = valid.clone();
    relative.projects[0]
        .allowed_directories
        .push(AllowedDirectory {
            path: "relative".into(),
            trust: TrustState::Approved,
        });
    let mut missing = valid;
    let missing_path = missing.projects[0].repo_root.join("missing");
    missing.projects[0]
        .allowed_directories
        .push(AllowedDirectory {
            path: missing_path,
            trust: TrustState::Unapproved,
        });

    // When: malformed JSON crosses the public load boundary.
    let results = [unknown, duplicate, relative, missing].map(|state| {
        let json = serde_json::to_string(&state).expect("fixture must serialize");
        sidebar_from_json(&json)
    });

    // Then: referential and path trust violations remain typed.
    assert!(matches!(&results[0], Err(SidebarError::Validation(_))));
    assert!(matches!(&results[1], Err(SidebarError::Validation(_))));
    assert!(matches!(&results[2], Err(SidebarError::Validation(_))));
    assert!(matches!(&results[3], Err(SidebarError::Validation(_))));
}

#[test]
fn sidebar_fixture_types_are_public_and_serializable() {
    // Given: public project and thread record constructors used by future UI adapters.
    let project = ProjectRecord {
        id: ProjectId::new("p1"),
        name: "One".to_owned(),
        repo_root: "/tmp/project".into(),
        allowed_directories: Vec::new(),
    };
    let thread = ThreadRecord::new(ThreadId::new("t1"), project.id.clone(), "Thread");

    // When: records are embedded without renderer/runtime types.
    let state = SidebarState {
        projects: vec![project],
        threads: vec![thread],
        ..SidebarState::default()
    };

    // Then: serde accepts the framework-independent shape.
    assert!(serde_json::to_value(state).is_ok());
}
