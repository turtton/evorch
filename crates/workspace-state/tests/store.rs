use serde_json::json;
use tempfile::tempdir;
use workspace_state::{
    ProjectId, SidebarDocument, SidebarState, SidebarStore, StoreError, ThreadId, ThreadRecord,
};

fn document_with_threads(ids: &[&str]) -> (SidebarDocument, tempfile::TempDir) {
    let mut sidebar = SidebarState::default();
    let dir = tempdir().unwrap();
    sidebar
        .add_project(ProjectId::new("p1"), "p", dir.path())
        .unwrap();
    for id in ids {
        sidebar.threads.push(ThreadRecord::new(
            ThreadId::new(*id),
            ProjectId::new("p1"),
            *id,
        ));
    }
    sidebar.refresh_derived();
    let document = SidebarDocument {
        sidebar,
        rest: serde_json::Map::new(),
    };
    (document, dir)
}

#[test]
fn fields_the_core_does_not_interpret_survive_a_write() {
    let dir = tempdir().unwrap();
    let store = SidebarStore::new(dir.path().join("sidebar.json"));
    let lock = store.lock().unwrap();
    let (mut document, _root) = document_with_threads(&["t1"]);
    document.rest.insert(
        "user_messages".into(),
        json!([{"id": "m1", "thread_id": "t1"}]),
    );
    document.rest.insert(
        "composer_drafts".into(),
        json!([{"draft_id": "d", "thread_id": "t1"}]),
    );
    store.write(&lock, &document).unwrap();
    let mut read = store.read(&lock).unwrap();
    assert_eq!(read.rest, document.rest);
    read.sidebar.threads[0].title = "renamed".into();
    store.write(&lock, &read).unwrap();
    let again = store.read(&lock).unwrap();
    assert_eq!(again.rest, document.rest);
    assert_eq!(again.sidebar.threads[0].title, "renamed");
}

#[test]
fn a_second_writer_is_told_busy_and_an_unreadable_file_is_left_alone() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sidebar.json");
    let store = SidebarStore::new(&path);
    let held = store.lock().unwrap();
    assert!(matches!(
        SidebarStore::new(&path).lock(),
        Err(StoreError::Busy)
    ));
    drop(held);
    std::fs::write(&path, b"{ not json").unwrap();
    let lock = store.lock().unwrap();
    assert!(matches!(store.read(&lock), Err(StoreError::Unreadable(_))));
    assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
}

#[test]
fn pruning_removes_a_tombstoned_thread_with_its_family_messages_and_selection() {
    let (mut document, _root) = document_with_threads(&["t1", "t2"]);
    document.sidebar.active_thread = Some(ThreadId::new("t1"));
    document
        .sidebar
        .set_family_pinned(&ThreadId::new("t1"), true)
        .unwrap();
    document.rest.insert(
        "user_messages".into(),
        json!([{"id": "m1", "thread_id": "t1"}, {"id": "m2", "thread_id": "t2"}]),
    );
    let removed = document.prune_threads(&|id| id == "t1");
    assert_eq!(removed, 1);
    assert!(
        document
            .sidebar
            .families
            .iter()
            .all(|f| f.key.to_string() != "t1")
    );
    assert_eq!(
        document.rest["user_messages"],
        json!([{"id": "m2", "thread_id": "t2"}])
    );
    assert_eq!(document.sidebar.active_thread, Some(ThreadId::new("t2")));
}
