use std::path::PathBuf;

use tempfile::{TempDir, tempdir};
use workspace_ui::{
    ProjectId, SidebarError, SidebarState, ThreadError, ThreadId, ThreadRecord, sidebar_from_json,
    sidebar_to_json,
};

fn sidebar() -> (TempDir, SidebarState) {
    let directory = tempdir().expect("temporary directory must be created");
    let root = directory.path().join("project");
    std::fs::create_dir(&root).expect("project directory must be created");
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("project");
    sidebar
        .add_project(project_id.clone(), "Project", &root)
        .expect("project must be added");
    sidebar
        .create_thread(ThreadId::new("thread"), project_id, "Thread")
        .expect("thread must be added");
    (directory, sidebar)
}

#[test]
fn old_thread_json_without_extra_read_dirs_deserializes() {
    let (_project, state) = sidebar();
    let mut value = serde_json::to_value(state).expect("sidebar serializes");
    value["threads"][0]
        .as_object_mut()
        .expect("thread is an object")
        .remove("extra_read_dirs");

    let restored: SidebarState =
        serde_json::from_value(value).expect("legacy thread record deserializes");
    assert!(restored.threads[0].extra_read_dirs.is_empty());
}

#[test]
fn empty_extra_read_dirs_are_omitted_from_json() {
    let thread = ThreadRecord::new(ThreadId::new("thread"), ProjectId::new("project"), "Thread");
    let json = serde_json::to_value(thread).expect("thread serializes");
    assert!(json.get("extra_read_dirs").is_none());
}

#[test]
fn setter_canonicalizes_deduplicates_and_rejects_too_broad_or_missing_paths() {
    let (_project, mut state) = sidebar();
    let thread_id = ThreadId::new("thread");
    let directories = tempdir().expect("temporary directory must be created");
    let read_dir = directories.path().join("read");
    std::fs::create_dir(&read_dir).expect("read directory must be created");
    let noncanonical = read_dir.join("..").join("read");

    state
        .set_thread_extra_read_dirs(&thread_id, &[noncanonical.clone(), read_dir.clone()])
        .expect("existing paths are canonicalized and duplicates removed");
    assert_eq!(
        state.threads[0].extra_read_dirs,
        [read_dir.canonicalize().unwrap()]
    );

    assert!(matches!(
        state.set_thread_extra_read_dirs(&thread_id, &[PathBuf::from("/")]),
        Err(ThreadError::InvalidReadDirectory(_))
    ));
    if let Some(home) = std::env::var_os("HOME") {
        assert!(matches!(
            state.set_thread_extra_read_dirs(&thread_id, &[PathBuf::from(home)]),
            Err(ThreadError::InvalidReadDirectory(_))
        ));
    }
    let missing = directories.path().join("missing");
    assert!(matches!(
        state.set_thread_extra_read_dirs(&thread_id, &[missing]),
        Err(ThreadError::InvalidReadDirectory(_))
    ));
    assert!(matches!(
        state.set_thread_extra_read_dirs(&thread_id, &[PathBuf::from("relative")]),
        Err(ThreadError::InvalidReadDirectory(_))
    ));
}

#[test]
fn validate_rejects_relative_extra_read_directory_but_allows_missing_absolute_path() {
    let (_project, mut state) = sidebar();
    state.threads[0].extra_read_dirs = vec![PathBuf::from("relative")];
    assert!(matches!(state.validate(), Err(SidebarError::Validation(_))));

    state.threads[0].extra_read_dirs = vec![PathBuf::from("/missing-extra-read-directory")];
    assert!(state.validate().is_ok());
}

#[test]
fn setter_rejects_unknown_thread() {
    let (_project, mut state) = sidebar();
    assert_eq!(
        state.set_thread_extra_read_dirs(&ThreadId::new("missing"), &[]),
        Err(ThreadError::UnknownThread)
    );
}

#[test]
fn sidebar_json_round_trip_preserves_extra_read_dirs() {
    let (_project, mut state) = sidebar();
    let directory = tempdir().expect("temporary directory must be created");
    let read_dir = directory
        .path()
        .canonicalize()
        .expect("directory canonicalizes");
    state.threads[0].extra_read_dirs.push(read_dir.clone());
    let json = sidebar_to_json(&state).expect("sidebar serializes");
    let restored = sidebar_from_json(&json).expect("sidebar deserializes");
    assert_eq!(restored.threads[0].extra_read_dirs, [read_dir]);
}

#[test]
fn project_ancestors_root_and_descendants_are_rejected_atomically() {
    let (_project, mut state) = sidebar();
    let root = state.projects[0].repo_root.clone();
    let inside = root.join("drafts");
    std::fs::create_dir(&inside).unwrap();
    let external = tempdir().unwrap();
    let id = ThreadId::new("thread");
    state
        .set_thread_extra_read_dirs(&id, &[external.path().to_owned()])
        .unwrap();
    let before = state.threads[0].extra_read_dirs.clone();
    for invalid in [root.parent().unwrap().to_owned(), root, inside] {
        assert!(
            state
                .set_thread_extra_read_dirs(&id, &[external.path().to_owned(), invalid])
                .is_err()
        );
        assert_eq!(state.threads[0].extra_read_dirs, before);
    }
}
