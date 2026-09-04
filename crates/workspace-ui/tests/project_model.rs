use std::path::PathBuf;

use tempfile::tempdir;
use workspace_ui::{Membership, ProjectError, ProjectId, SidebarState, TrustState};

#[test]
fn add_project_canonicalizes_and_rejects_duplicate_root() {
    // Given: an existing directory and an empty sidebar.
    let directory = tempdir().expect("temporary directory must be created");
    let root = directory.path().join("project");
    std::fs::create_dir(&root).expect("project directory must be created");
    let mut sidebar = SidebarState::default();

    // When: the same root is added under two project IDs.
    sidebar
        .add_project(ProjectId::new("p1"), "One", &root)
        .expect("first project must be added");
    let duplicate = sidebar.add_project(ProjectId::new("p2"), "Two", &root);

    // Then: the stored root is canonical and the duplicate is rejected.
    assert_eq!(
        sidebar.projects[0].repo_root,
        root.canonicalize().expect("root canonicalizes")
    );
    assert_eq!(duplicate, Err(ProjectError::DuplicateProject));
}

#[test]
fn allowed_directory_rejects_relative_missing_and_nested_paths() {
    // Given: a project with an existing allowed directory.
    let directory = tempdir().expect("temporary directory must be created");
    let root = directory.path().join("project");
    let allowed = directory.path().join("allowed");
    std::fs::create_dir(&root).expect("project directory must be created");
    std::fs::create_dir(&allowed).expect("allowed directory must be created");
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("p1");
    sidebar
        .add_project(project_id.clone(), "One", &root)
        .expect("project must be added");
    sidebar
        .add_allowed_directory(&project_id, &allowed, TrustState::Approved)
        .expect("allowed directory must be added");
    let nested_root = root.join("nested");
    let nested_allowed = allowed.join("nested");
    std::fs::create_dir(&nested_root).expect("nested project directory must be created");
    std::fs::create_dir(&nested_allowed).expect("nested allowed directory must be created");

    // When: invalid directory forms are added.
    let relative = sidebar.add_allowed_directory(
        &project_id,
        PathBuf::from("relative").as_path(),
        TrustState::Unapproved,
    );
    let missing = sidebar.add_allowed_directory(
        &project_id,
        &directory.path().join("missing"),
        TrustState::Unapproved,
    );
    let in_root = sidebar.add_allowed_directory(&project_id, &nested_root, TrustState::Unapproved);
    let in_allowed =
        sidebar.add_allowed_directory(&project_id, &nested_allowed, TrustState::Unapproved);

    // Then: each boundary failure remains typed.
    assert_eq!(relative, Err(ProjectError::NotAbsolute));
    assert_eq!(
        missing,
        Err(ProjectError::Canonicalize(
            directory.path().join("missing").display().to_string()
        ))
    );
    assert_eq!(in_root, Err(ProjectError::NestedInProjectRoot));
    assert_eq!(in_allowed, Err(ProjectError::NestedInExistingAllowed));
}

#[test]
fn membership_resolves_root_runtime_worktree_allowed_and_outside() {
    // Given: a project root, runtime worktree, explicit allowed dir, and unrelated sibling.
    let directory = tempdir().expect("temporary directory must be created");
    let root = directory.path().join("project");
    let run_dir = root.join(".evorch/worktrees/run-7");
    let allowed = directory.path().join("allowed");
    let outside = directory.path().join("outside");
    for path in [&run_dir, &allowed, &outside] {
        std::fs::create_dir_all(path).expect("fixture directory must be created");
    }
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("p1");
    sidebar
        .add_project(project_id.clone(), "One", &root)
        .expect("project must be added");
    sidebar
        .add_allowed_directory(&project_id, &allowed, TrustState::Approved)
        .expect("allowed directory must be added");
    let project = &sidebar.projects[0];

    // When: membership is resolved for each canonical path class.
    // Then: runtime worktrees are auto-allowed while explicit directories retain trust.
    assert_eq!(
        project.resolve_membership(&project.repo_root),
        Membership::ProjectRoot
    );
    assert_eq!(
        project.resolve_membership(&run_dir),
        Membership::RuntimeWorktree { run_dir }
    );
    assert_eq!(
        project.resolve_membership(&allowed),
        Membership::AllowedDirectory {
            trust: TrustState::Approved
        }
    );
    assert_eq!(project.resolve_membership(&outside), Membership::Outside);
}

#[test]
fn default_trust_is_unapproved() {
    // Given: the trust enum's default boundary value.
    // When: Default is evaluated.
    let trust = TrustState::default();

    // Then: access fails closed until explicitly approved.
    assert_eq!(trust, TrustState::Unapproved);
}

#[test]
fn set_allowed_trust_updates_existing_canonical_directory() {
    // Given: an unapproved canonical allowed directory on a known project.
    let directory = tempdir().expect("temporary directory must be created");
    let root = directory.path().join("project");
    let allowed = directory.path().join("allowed");
    std::fs::create_dir(&root).expect("project directory must be created");
    std::fs::create_dir(&allowed).expect("allowed directory must be created");
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("p1");
    sidebar
        .add_project(project_id.clone(), "One", &root)
        .expect("project must be added");
    sidebar
        .add_allowed_directory(&project_id, &allowed, TrustState::Unapproved)
        .expect("allowed directory must be added");
    let canonical = allowed.canonicalize().expect("allowed path canonicalizes");

    // When: trust is approved through the public sidebar mutation API.
    sidebar
        .set_allowed_trust(&project_id, &canonical, TrustState::Approved)
        .expect("allowed directory trust must update");

    // Then: membership reports the updated trust state.
    assert_eq!(
        sidebar.projects[0].resolve_membership(&canonical),
        Membership::AllowedDirectory {
            trust: TrustState::Approved
        }
    );
}

#[test]
fn set_allowed_trust_rejects_unknown_and_noncanonical_paths() {
    // Given: one known project and allowed directory plus unrelated existing paths.
    let directory = tempdir().expect("temporary directory must be created");
    let root = directory.path().join("project");
    let allowed = directory.path().join("allowed");
    let unknown = directory.path().join("unknown");
    std::fs::create_dir(&root).expect("project directory must be created");
    std::fs::create_dir(&allowed).expect("allowed directory must be created");
    std::fs::create_dir(&unknown).expect("unknown directory must be created");
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("p1");
    sidebar
        .add_project(project_id.clone(), "One", &root)
        .expect("project must be added");
    sidebar
        .add_allowed_directory(&project_id, &allowed, TrustState::Unapproved)
        .expect("allowed directory must be added");
    let canonical = allowed.canonicalize().expect("allowed path canonicalizes");
    let noncanonical = canonical.join("..").join("allowed");

    // When: invalid project and path identities are submitted.
    let unknown_project =
        sidebar.set_allowed_trust(&ProjectId::new("missing"), &canonical, TrustState::Approved);
    let unknown_path = sidebar.set_allowed_trust(&project_id, &unknown, TrustState::Approved);
    let relative = sidebar.set_allowed_trust(
        &project_id,
        PathBuf::from("relative").as_path(),
        TrustState::Approved,
    );
    let noncanonical_result =
        sidebar.set_allowed_trust(&project_id, &noncanonical, TrustState::Approved);

    // Then: each boundary failure remains a distinct typed project error.
    assert_eq!(unknown_project, Err(ProjectError::UnknownProject));
    assert_eq!(unknown_path, Err(ProjectError::UnknownAllowedDirectory));
    assert_eq!(relative, Err(ProjectError::NotAbsolute));
    assert_eq!(noncanonical_result, Err(ProjectError::NotCanonical));
}
