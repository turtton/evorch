use tempfile::tempdir;
use workspace_ui::{
    AllowedDirectory, ProjectError, ProjectId, ProjectRecord, SidebarError, SidebarState, ThreadId,
    ThreadRecord, TrustState, load_sidebar, save_sidebar, sidebar_from_json, sidebar_to_json,
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
    assert_eq!(
        results[2],
        Err(SidebarError::Project(ProjectError::NotAbsolute))
    );
    assert!(matches!(
        &results[3],
        Err(SidebarError::Project(ProjectError::Canonicalize(_)))
    ));
}

#[test]
fn load_rejects_noncanonical_allowed_directory() {
    // Given: persisted state whose allowed path resolves through a parent component.
    let mut sidebar = valid_sidebar();
    let directory = tempdir().expect("temporary directory must be created");
    let allowed = directory.path().join("allowed");
    std::fs::create_dir(&allowed).expect("allowed directory must be created");
    sidebar.projects[0]
        .allowed_directories
        .push(AllowedDirectory {
            path: allowed.join("..").join("allowed"),
            trust: TrustState::Approved,
        });
    let json = serde_json::to_string(&sidebar).expect("fixture must serialize");

    // When: tampered JSON crosses the public load boundary.
    let result = sidebar_from_json(&json);

    // Then: canonical identity is required rather than silently normalizing persisted trust.
    assert_eq!(
        result,
        Err(SidebarError::Project(ProjectError::NotCanonical))
    );
}

#[test]
fn load_rejects_allowed_directory_inside_project_root() {
    // Given: persisted state that stores a runtime worktree as an explicit allowed directory.
    let mut sidebar = valid_sidebar();
    let worktree = sidebar.projects[0]
        .repo_root
        .join(".evorch/worktrees/run-1");
    std::fs::create_dir_all(&worktree).expect("runtime worktree must be created");
    sidebar.projects[0]
        .allowed_directories
        .push(AllowedDirectory {
            path: worktree,
            trust: TrustState::Approved,
        });
    let json = serde_json::to_string(&sidebar).expect("fixture must serialize");

    // When: tampered JSON crosses the public load boundary.
    let result = sidebar_from_json(&json);

    // Then: project-root descendants remain auto-allowed only through membership resolution.
    assert_eq!(
        result,
        Err(SidebarError::Project(ProjectError::NestedInProjectRoot))
    );
}

#[test]
fn load_rejects_nested_allowed_directories() {
    // Given: persisted state with one allowed directory contained by another.
    let mut sidebar = valid_sidebar();
    let directory = tempdir().expect("temporary directory must be created");
    let parent = directory.path().join("allowed");
    let child = parent.join("nested");
    std::fs::create_dir_all(&child).expect("allowed directories must be created");
    sidebar.projects[0].allowed_directories = vec![
        AllowedDirectory {
            path: parent,
            trust: TrustState::Approved,
        },
        AllowedDirectory {
            path: child,
            trust: TrustState::Unapproved,
        },
    ];
    let json = serde_json::to_string(&sidebar).expect("fixture must serialize");

    // When: tampered JSON crosses the public load boundary.
    let result = sidebar_from_json(&json);

    // Then: overlapping trust roots are rejected fail-closed.
    assert_eq!(
        result,
        Err(SidebarError::Project(ProjectError::NestedInExistingAllowed))
    );
}

#[test]
fn load_accepts_canonical_external_allowed_directory() {
    // Given: persisted state with one canonical directory outside the project root.
    let mut sidebar = valid_sidebar();
    let directory = tempdir().expect("temporary directory must be created");
    let allowed = directory.path().join("allowed");
    std::fs::create_dir(&allowed).expect("allowed directory must be created");
    let canonical = allowed.canonicalize().expect("allowed path canonicalizes");
    sidebar.projects[0]
        .allowed_directories
        .push(AllowedDirectory {
            path: canonical.clone(),
            trust: TrustState::Approved,
        });
    let json = serde_json::to_string(&sidebar).expect("fixture must serialize");

    // When: valid JSON crosses the public load boundary.
    let loaded = sidebar_from_json(&json).expect("valid sidebar must load");

    // Then: the external trust path and state are preserved.
    assert_eq!(loaded.projects[0].allowed_directories[0].path, canonical);
    assert_eq!(
        loaded.projects[0].allowed_directories[0].trust,
        TrustState::Approved
    );
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
