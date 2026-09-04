use std::collections::BTreeMap;

use tempfile::tempdir;
use workspace_ui::{ProjectId, SidebarState, ThreadError, ThreadId, ThreadRunPhase, ThreadState};

#[test]
fn create_switch_pin_unpin_threads_under_project() {
    // Given: a sidebar containing one project.
    let directory = tempdir().expect("temporary directory must be created");
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("p1");
    sidebar
        .add_project(project_id.clone(), "One", directory.path())
        .expect("project must be added");

    // When: two threads are created, switched, pinned, and unpinned.
    let first = ThreadId::new("t1");
    let second = ThreadId::new("t2");
    sidebar
        .create_thread(first.clone(), project_id.clone(), "First")
        .expect("first thread must be created");
    sidebar
        .create_thread(second.clone(), project_id, "Second")
        .expect("second thread must be created");
    sidebar
        .switch_thread(&second)
        .expect("second thread must be selected");
    sidebar
        .set_pinned(&first, true)
        .expect("first thread must pin");
    sidebar
        .set_pinned(&first, false)
        .expect("first thread must unpin");

    // Then: selection and pin state reflect the operator actions.
    assert_eq!(sidebar.active_thread, Some(second));
    assert!(!sidebar.threads[0].pinned);
}

#[test]
fn thread_state_precedence_paused_error_running_waiting_done_active() {
    // Given: a thread with two run IDs and each possible runtime phase combination.
    let mut thread =
        workspace_ui::ThreadRecord::new(ThreadId::new("t1"), ProjectId::new("p1"), "Thread");
    assert_eq!(thread.state(&BTreeMap::new()), ThreadState::Active);
    thread.run_ids = vec!["r1".to_owned(), "r2".to_owned()];

    // When: phase maps exercise precedence from done through error and operator pause.
    let done = BTreeMap::from([
        ("r1".to_owned(), ThreadRunPhase::Done),
        ("r2".to_owned(), ThreadRunPhase::Done),
    ]);
    let waiting = BTreeMap::from([
        ("r1".to_owned(), ThreadRunPhase::Done),
        ("r2".to_owned(), ThreadRunPhase::Waiting),
    ]);
    let running = BTreeMap::from([
        ("r1".to_owned(), ThreadRunPhase::Waiting),
        ("r2".to_owned(), ThreadRunPhase::Pending),
    ]);
    let error = BTreeMap::from([
        ("r1".to_owned(), ThreadRunPhase::Running),
        ("r2".to_owned(), ThreadRunPhase::Error),
    ]);

    // Then: paused is operator-owned and runtime states follow the specified precedence.
    assert_eq!(thread.state(&done), ThreadState::Done);
    assert_eq!(thread.state(&waiting), ThreadState::Waiting);
    assert_eq!(thread.state(&running), ThreadState::Running);
    assert_eq!(thread.state(&error), ThreadState::Error);
    thread.paused = true;
    assert_eq!(thread.state(&error), ThreadState::Paused);
}

#[test]
fn thread_creation_rejects_unknown_project() {
    // Given: an empty sidebar.
    let mut sidebar = SidebarState::default();

    // When: a thread references an unknown project.
    let result = sidebar.create_thread(ThreadId::new("t1"), ProjectId::new("missing"), "Thread");

    // Then: creation fails closed with a typed error.
    assert_eq!(result, Err(ThreadError::UnknownProject));
}
