//! Threads created from, or shown in place of, another take its archive and pin
//! state from the newest shared sidebar, or the action fails.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use event_bus::{EscalationMemoSummary, Event, LifecycleEvent};
use workspace_ui::{LineageKind, ThreadId, UiSettings};

use super::WorkbenchState;
use super::delivery_tests::state;
use crate::fixture::DemoSource;

fn window_on(path: &Path) -> WorkbenchState<DemoSource> {
    WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(workspace_ui::load_sidebar(path).unwrap())
        .with_sidebar_path(path.into())
}

fn series(state: &WorkbenchState<DemoSource>, id: &str) -> (bool, Option<i64>, u64) {
    let thread = state
        .sidebar()
        .threads
        .iter()
        .find(|thread| thread.id.to_string() == id)
        .unwrap_or_else(|| panic!("thread {id} must exist"));
    (thread.archived, thread.archived_at, thread.archive_revision)
}

fn lock_file(path: &Path) -> std::fs::File {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_file_name(name))
        .unwrap();
    file.try_lock().unwrap();
    file
}

fn escalation(source: &str, new: &str) -> Event {
    Event::new(LifecycleEvent::EscalationRequested {
        source_run_id: source.into(),
        new_run_id: new.into(),
        summary: EscalationMemoSummary {
            original_request: "request".into(),
            escalation_reason: "reason".into(),
            files_touched: Vec::new(),
            blockers: Vec::new(),
            suggested_next: "next".into(),
        },
    })
}

/// Window A holds `thread` with a run; the file exists; returns both windows.
fn two_windows(
    directory: &Path,
) -> (
    WorkbenchState<DemoSource>,
    WorkbenchState<DemoSource>,
    PathBuf,
) {
    let path = directory.join("sidebar.json");
    let mut a = state(&path);
    a.sidebar.threads[0].run_ids = vec!["run-1".into()];
    a.save_sidebar();
    let b = window_on(&path);
    (a, b, path)
}

#[test]
fn a_stale_window_switching_versions_takes_the_archive_state_from_the_newest_sidebar() {
    // Given: a conversation with a rewound version, shown by both windows.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let v1 = ThreadId::new("thread");
    let v2 = a.branch_thread(&v1, None, LineageKind::Rewind).unwrap();
    let mut b = window_on(&path);

    // When: A archives the conversation, and B (which never saw that) switches version.
    a.toggle_archive(v2.clone()).unwrap();
    assert!(series(&a, "thread").0 || series(&a, &v2.to_string()).0);
    b.switch_version(v1.clone()).unwrap();

    // Then: the version B shows has the newest archive state, in B and in the file.
    let on_disk = workspace_ui::load_sidebar(&path).unwrap();
    let of = |sidebar: &workspace_ui::SidebarState, id: &ThreadId| {
        let t = sidebar.threads.iter().find(|t| &t.id == id).unwrap();
        (t.archived, t.archived_at, t.archive_revision)
    };
    assert_eq!(of(&on_disk, &v1), of(&on_disk, &v2));
    assert_eq!(of(b.sidebar(), &v1), of(&on_disk, &v1));
    assert!(of(&on_disk, &v1).0);
    assert!(
        !on_disk
            .threads
            .iter()
            .find(|t| t.id == v1)
            .unwrap()
            .superseded
    );
}

#[test]
fn a_locked_sidebar_leaves_a_version_switch_and_a_rewind_undone() {
    // Given: a conversation with a rewound version, and another process holding the lock.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let v1 = ThreadId::new("thread");
    let v2 = a.branch_thread(&v1, None, LineageKind::Rewind).unwrap();
    let before = a.sidebar().clone();
    let saved = std::fs::read(&path).unwrap();
    let lock = lock_file(&path);

    // When: switching versions and rewinding again.
    let switched = a.switch_version(v1);
    let rewound = a.branch_thread(&v2, None, LineageKind::Rewind);

    // Then: both fail and neither the window nor the file changed.
    assert!(switched.is_err() && rewound.is_err());
    assert_eq!(a.sidebar(), &before);
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    drop(lock);
}

#[test]
fn an_escalated_thread_takes_the_parents_newest_state() {
    // Given: window B's parent is stale; A archived it.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, mut b, path) = two_windows(directory.path());
    a.toggle_archive(ThreadId::new("thread")).unwrap();

    // When: B folds an escalation of the parent's run.
    b.apply_events([escalation("run-1", "run-2")]);

    // Then: the child is archived like the parent, in B and in the file.
    let on_disk = workspace_ui::load_sidebar(&path).unwrap();
    let parent = series(&a, "thread");
    assert!(parent.0);
    let child = series(&b, "escalation-run-2");
    assert_eq!((child.0, child.1), (parent.0, parent.1));
    assert!(child.2 > 0, "the settled state is a numbered edit");
    let saved = on_disk
        .threads
        .iter()
        .find(|t| t.id.to_string() == "escalation-run-2")
        .unwrap();
    assert_eq!(
        (saved.archived, saved.archived_at, saved.archive_revision),
        child
    );
    assert!(b.pending_family.threads.is_empty());
}

#[test]
fn an_escalated_thread_created_under_a_lock_is_settled_later_and_mirrored_only_then() {
    // Given: a mirroring window whose parent was archived elsewhere, and a locked sidebar.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let config = storage::StorageConfig {
        db_path: directory.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let mut b = window_on(&path).with_thread_mirror(storage.handle());
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    let lock = lock_file(&path);

    // When: the escalation is folded while the lock is held.
    b.apply_events([escalation("run-1", "run-2")]);
    b.sync_thread_mirror(Instant::now());
    let barrier = storage::threads::ThreadUpsert {
        thread_id: "barrier".into(),
        created_at_ns: 1,
        updated_at_ns: 1,
        ..Default::default()
    };
    storage.handle().upsert_threads_host(&[barrier]).unwrap();

    // Then: the thread exists in the neutral state, waits to be settled and is not
    // in the database.
    assert_eq!(series(&b, "escalation-run-2"), (false, None, 0));
    assert_eq!(b.pending_family.threads.len(), 1);
    let database = storage::Database::open(&config).unwrap();
    assert!(database.thread("escalation-run-2").unwrap().is_none());

    // When: the lock is free again and the retry runs.
    drop(lock);
    b.retry_pending_family_state(Instant::now() + Duration::from_secs(5));
    storage
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "barrier".into(),
            created_at_ns: 1,
            updated_at_ns: 1,
            ..Default::default()
        }])
        .unwrap();

    // Then: the child follows the parent and reaches the database in that state.
    assert!(b.pending_family.threads.is_empty());
    let (archived, archived_at, _) = series(&b, "escalation-run-2");
    assert_eq!((archived, archived_at), {
        let parent = series(&a, "thread");
        (parent.0, parent.1)
    });
    let row = storage::Database::open(&config)
        .unwrap()
        .thread("escalation-run-2")
        .unwrap()
        .expect("mirrored once settled");
    assert!(row.archived);
}
