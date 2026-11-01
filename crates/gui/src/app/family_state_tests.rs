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
    assert!(!pending(&b, "escalation-run-2"));
}

fn pending(state: &WorkbenchState<DemoSource>, id: &str) -> bool {
    state
        .sidebar()
        .threads
        .iter()
        .find(|thread| thread.id.to_string() == id)
        .unwrap_or_else(|| panic!("thread {id} must exist"))
        .state_pending
}

/// A mirroring window on the shared file, with the database it mirrors into.
fn mirroring(
    path: &Path,
    directory: &Path,
) -> (
    WorkbenchState<DemoSource>,
    storage::Storage,
    storage::StorageConfig,
) {
    let config = storage::StorageConfig {
        db_path: directory.join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let window = window_on(path).with_thread_mirror(storage.handle());
    (window, storage, config)
}

/// Waits until the writer has handled everything sent before (it runs commands in order).
fn barrier(storage: &storage::Storage) {
    storage
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "barrier".into(),
            created_at_ns: 1,
            updated_at_ns: 1,
            ..Default::default()
        }])
        .unwrap();
}

fn row(config: &storage::StorageConfig, id: &str) -> Option<storage::threads::ThreadRow> {
    storage::Database::open(config).unwrap().thread(id).unwrap()
}

#[test]
fn an_escalated_thread_created_under_a_lock_is_pending_unmirrored_and_settled_later() {
    // Given: a mirroring window whose parent was archived elsewhere, and a locked sidebar.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let (mut b, storage, config) = mirroring(&path, directory.path());
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    let lock = lock_file(&path);

    // When: the escalation is folded while the lock is held, and the mirror ticks.
    b.apply_events([escalation("run-1", "run-2")]);
    b.sync_thread_mirror(Instant::now());
    barrier(&storage);

    // Then: the thread exists in the neutral state, is flagged and is not in the database.
    assert_eq!(series(&b, "escalation-run-2"), (false, None, 0));
    assert!(pending(&b, "escalation-run-2"));
    assert!(row(&config, "escalation-run-2").is_none());

    // When: the lock is free again and the retry runs.
    drop(lock);
    b.retry_pending_family_state(Instant::now() + Duration::from_secs(5));
    barrier(&storage);

    // Then: the child follows the parent and reaches the database in that state.
    assert!(!pending(&b, "escalation-run-2"));
    let parent = series(&a, "thread");
    let child = series(&b, "escalation-run-2");
    assert_eq!((child.0, child.1), (parent.0, parent.1));
    assert!(
        row(&config, "escalation-run-2")
            .expect("mirrored once settled")
            .archived
    );
}

#[test]
fn a_pending_thread_saved_before_the_retry_is_settled_by_a_restarted_or_another_window() {
    // Given: the escalation was folded under a lock; then the lock was freed and an
    // ordinary save wrote the pending child before any retry ran.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let mut b = window_on(&path);
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    let lock = lock_file(&path);
    b.apply_events([escalation("run-1", "run-2")]);
    drop(lock);
    b.save_sidebar();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("\"state_pending\": true"), "{saved}");
    drop(b);

    // When: another window (a restart, or a window of another process) loads the file
    // and mirrors.
    let (mut c, storage, config) = mirroring(&path, directory.path());
    assert!(pending(&c, "escalation-run-2"));
    c.sync_thread_mirror(Instant::now());
    barrier(&storage);

    // Then: the pending record is held back from the database until it settles it.
    assert!(row(&config, "escalation-run-2").is_none());
    c.retry_pending_family_state(Instant::now() + Duration::from_secs(5));
    barrier(&storage);
    assert!(!pending(&c, "escalation-run-2"));
    assert!(row(&config, "escalation-run-2").expect("mirrored").archived);
    // And: the file no longer carries the flag.
    let settled = workspace_ui::load_sidebar(&path).unwrap();
    assert!(settled.threads.iter().all(|thread| !thread.state_pending));
}

#[test]
fn several_pending_threads_stay_unmirrored_until_one_transaction_settles_all_of_them() {
    // Given: two escalated children created under a lock, a retry that still cannot lock.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let (mut b, storage, config) = mirroring(&path, directory.path());
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    let lock = lock_file(&path);
    b.apply_events([escalation("run-1", "run-2"), escalation("run-1", "run-3")]);
    b.retry_pending_family_state(Instant::now() + Duration::from_secs(5));
    b.sync_thread_mirror(Instant::now());
    barrier(&storage);
    assert!(pending(&b, "escalation-run-2") && pending(&b, "escalation-run-3"));
    assert!(row(&config, "escalation-run-2").is_none());
    assert!(row(&config, "escalation-run-3").is_none());

    // When: the lock is free and one retry runs.
    drop(lock);
    b.retry_pending_family_state(Instant::now() + Duration::from_secs(10));
    barrier(&storage);

    // Then: both are settled in that one transaction and both are mirrored as archived.
    for id in ["escalation-run-2", "escalation-run-3"] {
        assert!(!pending(&b, id), "{id}");
        assert!(row(&config, id).expect("mirrored").archived, "{id}");
    }
}

#[test]
fn a_pending_thread_waits_for_its_own_pending_parent() {
    use workspace_ui::{ProjectId, ThreadRecord};
    let project = ProjectId::new("p");
    let mut parent = ThreadRecord::new(ThreadId::new("parent"), project.clone(), "parent");
    parent.set_archived(true, 5);
    let mut middle = ThreadRecord::new(ThreadId::new("middle"), project.clone(), "middle");
    middle.parent_thread_id = Some(parent.id.clone());
    middle.state_pending = true;
    let mut leaf = ThreadRecord::new(ThreadId::new("leaf"), project, "leaf");
    leaf.parent_thread_id = Some(middle.id.clone());
    leaf.state_pending = true;
    // The leaf comes first: it can only settle after its parent did.
    let mut threads = vec![leaf, middle, parent];

    super::thread_archive::settle_pending_threads(&mut threads);

    for thread in &threads {
        assert!(!thread.state_pending, "{}", thread.id);
        assert!(thread.archived, "{}", thread.id);
    }
}
