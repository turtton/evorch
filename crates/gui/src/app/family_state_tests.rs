//! Archive and pin state belong to a conversation family. Creating a member (fork,
//! rewind, escalation) or showing another version adds membership only; archive and
//! pin edits number the family's series under the save lock; the database projection
//! follows the committed family state of every member.

use std::path::{Path, PathBuf};
use std::time::Instant;

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

/// The archive series a thread resolves to in a window.
fn series(state: &WorkbenchState<DemoSource>, id: &str) -> (bool, Option<i64>, u64) {
    resolved(state.sidebar(), id)
}

fn resolved(sidebar: &workspace_ui::SidebarState, id: &str) -> (bool, Option<i64>, u64) {
    let state = &sidebar.resolve_states()[&ThreadId::new(id)];
    assert!(state.is_resolved(), "{id}: {:?}", state.family);
    (state.archived, state.archived_at, state.archive_revision)
}

fn pin_series(sidebar: &workspace_ui::SidebarState, id: &str) -> (bool, u64) {
    let state = &sidebar.resolve_states()[&ThreadId::new(id)];
    (state.pinned, state.pin_revision)
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
fn a_stale_window_switching_versions_changes_no_family_state() {
    // Given: a conversation with a rewound version, shown by both windows.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let v1 = ThreadId::new("thread");
    let v2 = a.branch_thread(&v1, None, LineageKind::Rewind).unwrap();
    let mut b = window_on(&path);

    // When: A archives the conversation, and B (which never saw that) switches version.
    a.toggle_archive(v2.clone()).unwrap();
    let archived = series(&a, "thread");
    assert!(archived.0 && archived.2 == 1);
    b.switch_version(v1.clone()).unwrap();

    // Then: the family state is the one A numbered, in the file and in B, whichever
    // version is shown; the switch numbered nothing.
    let on_disk = workspace_ui::load_sidebar(&path).unwrap();
    for id in ["thread", &v2.to_string()] {
        assert_eq!(resolved(&on_disk, id), archived, "{id}");
        assert_eq!(series(&b, id), archived, "{id}");
    }
    assert_eq!(on_disk.families.len(), 1);
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
fn an_escalated_thread_only_joins_the_family_and_reads_its_newest_state() {
    // Given: window B's parent is stale; A archived it.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, mut b, path) = two_windows(directory.path());
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    let parent = series(&a, "thread");

    // When: B folds an escalation of the parent's run, and saves.
    b.apply_events([escalation("run-1", "run-2")]);
    b.save_sidebar();
    b.import_shared_sidebar();

    // Then: the child is a member that reads the family's state; no state of its own
    // was made, and the family still has the one series A numbered.
    let on_disk = workspace_ui::load_sidebar(&path).unwrap();
    assert_eq!(resolved(&on_disk, "escalation-run-2"), parent);
    assert_eq!(series(&b, "escalation-run-2"), parent);
    assert_eq!(on_disk.families.len(), 1);
    assert_eq!(on_disk.families[0].archive_revision, 1);
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

/// What the database holds of a thread: `(archived, archive_revision, pinned, pin_revision)`.
fn projected(
    config: &storage::StorageConfig,
    id: &str,
) -> Option<(bool, Option<u64>, bool, Option<u64>)> {
    row(config, id).map(|row| {
        (
            row.archived,
            row.archive_revision,
            row.pinned,
            row.pin_revision,
        )
    })
}

#[test]
fn a_family_edit_reaches_every_member_of_the_database_and_a_new_member_gets_the_state() {
    // Given: a mirroring window with a fork, a rewound version and an escalation.
    let directory = tempfile::tempdir().unwrap();
    let (a, _, path) = two_windows(directory.path());
    drop(a);
    let (mut b, storage, config) = mirroring(&path, directory.path());
    let root = ThreadId::new("thread");
    let fork = b.branch_thread(&root, None, LineageKind::Fork).unwrap();
    let version = b.branch_thread(&root, None, LineageKind::Rewind).unwrap();
    b.apply_events([escalation("run-1", "run-2")]);
    b.force_thread_mirror();
    barrier(&storage);

    // When: the family is archived from one member, then pinned from another.
    b.toggle_archive(root.clone()).unwrap();
    b.sync_thread_mirror(Instant::now());
    barrier(&storage);
    b.toggle_pin(root.clone()).unwrap();
    b.force_thread_mirror();
    barrier(&storage);

    // Then: every member's row carries the family's two series.
    for id in [
        "thread",
        &fork.to_string(),
        &version.to_string(),
        "escalation-run-2",
    ] {
        assert_eq!(
            projected(&config, id),
            Some((true, Some(1), true, Some(1))),
            "{id}"
        );
    }

    // When: a member joins afterwards.
    let late = b.branch_thread(&fork, None, LineageKind::Fork).unwrap();
    b.force_thread_mirror();
    barrier(&storage);

    // Then: it carries the same series, not a number of its own.
    assert_eq!(
        projected(&config, &late.to_string()),
        Some((true, Some(1), true, Some(1)))
    );
}

#[test]
fn pinning_acts_on_the_whole_family_and_leaves_the_archive_series_alone() {
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let root = ThreadId::new("thread");
    let fork = a.branch_thread(&root, None, LineageKind::Fork).unwrap();
    a.apply_events([escalation("run-1", "run-2")]);
    a.save_sidebar();

    a.toggle_pin(root.clone()).unwrap();

    let on_disk = workspace_ui::load_sidebar(&path).unwrap();
    for id in ["thread", &fork.to_string(), "escalation-run-2"] {
        assert_eq!(pin_series(&on_disk, id), (true, 1), "{id}");
        assert_eq!(resolved(&on_disk, id), (false, None, 0), "{id}");
    }
    // A pinned family is not archived by an archive click on it.
    a.toggle_archive(root.clone()).unwrap();
    assert!(!series(&a, "thread").0);
    // And unpinning is a numbered edit of the same series.
    a.toggle_pin(root).unwrap();
    assert_eq!(
        pin_series(
            &workspace_ui::load_sidebar(&path).unwrap(),
            &fork.to_string()
        ),
        (false, 2)
    );
}

#[test]
fn an_archive_and_a_stale_pin_made_by_two_windows_both_survive() {
    // Given: neither window has a state for the family yet.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, mut b, path) = two_windows(directory.path());

    // When: A archives and the stale B pins, each creating the family's state.
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    b.toggle_pin(ThreadId::new("thread")).unwrap();

    // Then: one state, with both edits and each series' own number.
    let on_disk = workspace_ui::load_sidebar(&path).unwrap();
    assert_eq!(on_disk.families.len(), 1);
    assert_eq!(
        resolved(&on_disk, "thread"),
        (true, series(&a, "thread").1, 1)
    );
    assert_eq!(pin_series(&on_disk, "thread"), (true, 1));
}

#[test]
fn a_stale_window_replaying_a_known_member_never_rewinds_the_database() {
    // Given: A mirrors; it folded the escalation, archived the family, then restored it.
    let directory = tempfile::tempdir().unwrap();
    let (a, _, path) = two_windows(directory.path());
    drop(a);
    let (mut a, storage, config) = mirroring(&path, directory.path());
    let mut b = window_on(&path);
    a.apply_events([escalation("run-1", "run-2")]);
    a.save_sidebar();
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    a.force_thread_mirror();
    barrier(&storage);
    let after_restore = projected(&config, "escalation-run-2").unwrap();
    assert_eq!((after_restore.0, after_restore.1), (false, Some(2)));

    // When: the stale B (which never saw the child) replays the same escalation and
    // saves, then A archives the family again.
    b.apply_events([escalation("run-1", "run-2")]);
    b.save_sidebar();
    a.import_shared_sidebar();
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    a.force_thread_mirror();
    barrier(&storage);

    // Then: the series only moved forward, in the file and in the database.
    let on_disk = workspace_ui::load_sidebar(&path).unwrap();
    assert_eq!(resolved(&on_disk, "escalation-run-2").2, 3);
    assert_eq!(on_disk.families.len(), 1);
    for id in ["thread", "escalation-run-2"] {
        assert_eq!(
            projected(&config, id).map(|row| (row.0, row.1)),
            Some((true, Some(3))),
            "{id}"
        );
    }
}

/// Whether the database lists the thread among those search may return.
fn searchable(config: &storage::StorageConfig, id: &str) -> bool {
    let database = storage::Database::open(config).unwrap();
    let Some(row) = database.thread(id).unwrap() else {
        return false;
    };
    let root = row
        .project_root
        .expect("a mirrored thread has its project root");
    database
        .threads_in_project_visible(&root, None, 100)
        .unwrap()
        .threads
        .iter()
        .any(|listed| listed.thread_id == id)
}

#[test]
fn a_child_added_while_the_sidebar_is_locked_is_told_only_with_the_state_the_file_holds() {
    // Given: B (mirroring) started before A archived the family, so B's own view of the
    // family is the old, unarchived one; the shared file is then locked by someone.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let (mut b, storage, config) = mirroring(&path, directory.path());
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    let lock = lock_file(&path);

    // When: B folds an escalation (needs no lock), fails to branch and to switch
    // version, and mirrors while the lock is held.
    b.apply_events([escalation("run-1", "run-2")]);
    assert!(
        b.branch_thread(&ThreadId::new("escalation-run-2"), None, LineageKind::Fork)
            .is_err()
    );
    assert!(b.switch_version(ThreadId::new("thread")).is_err());
    b.force_thread_mirror();
    barrier(&storage);

    // Then: nothing about the unsaved child reached the database, in any state.
    assert_eq!(b.sidebar().threads.len(), 2);
    assert!(row(&config, "escalation-run-2").is_none());
    assert!(!searchable(&config, "escalation-run-2"));

    // When: the lock is free and B saves, which merges the file's newer state into the
    // file but is not taken into B yet (no import).
    drop(lock);
    b.save_sidebar();
    assert!(!series(&b, "thread").0, "B has not taken the file in yet");
    b.force_thread_mirror();
    barrier(&storage);

    // Then: the child reaches the database with the family state the file holds, so it
    // is out of search from its first row on; the family's rows agree.
    for id in ["thread", "escalation-run-2"] {
        assert_eq!(
            projected(&config, id).map(|row| (row.0, row.1)),
            Some((true, Some(1))),
            "{id}"
        );
        assert!(!searchable(&config, id), "{id}");
    }

    // When: B takes the file in.
    b.import_shared_sidebar();
    b.force_thread_mirror();
    barrier(&storage);

    // Then: B agrees, and the database did not change.
    assert_eq!(series(&b, "escalation-run-2"), series(&a, "thread"));
    assert_eq!(
        projected(&config, "escalation-run-2").map(|row| (row.0, row.1)),
        Some((true, Some(1)))
    );
}

#[test]
fn a_child_in_a_window_that_cannot_save_is_not_told_even_when_the_family_is_known() {
    // Given: a mirroring window whose family is already in the database, and a locked file.
    let directory = tempfile::tempdir().unwrap();
    let (a, _, path) = two_windows(directory.path());
    drop(a);
    let (mut b, storage, config) = mirroring(&path, directory.path());
    b.force_thread_mirror();
    barrier(&storage);
    let lock = lock_file(&path);

    // When: it adds a child and mirrors repeatedly while the lock is held.
    b.apply_events([escalation("run-1", "run-2")]);
    b.force_thread_mirror();
    b.force_thread_mirror();
    barrier(&storage);

    // Then: the child is unknown to the database until the save succeeds.
    assert!(row(&config, "escalation-run-2").is_none());
    drop(lock);
    b.save_sidebar();
    b.force_thread_mirror();
    barrier(&storage);
    assert!(row(&config, "escalation-run-2").is_some());
}

#[test]
fn an_edit_saved_but_never_mirrored_reaches_the_database_after_a_restart() {
    // Given: a window without a mirror archives a family with a member and saves; the
    // process then ends before anything reaches the database.
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    let fork = a
        .branch_thread(&ThreadId::new("thread"), None, LineageKind::Fork)
        .unwrap();
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    drop(a);

    // When: a restarted window mirrors what the file says.
    let (mut b, storage, config) = mirroring(&path, directory.path());
    b.force_thread_mirror();
    barrier(&storage);

    // Then: every member's row carries the family's committed state.
    for id in ["thread", &fork.to_string()] {
        assert_eq!(
            projected(&config, id).map(|row| (row.0, row.1)),
            Some((true, Some(1))),
            "{id}"
        );
    }

    // And: a second restart sends the same values again without moving the numbers.
    drop(b);
    let (mut c, storage, config) = mirroring(&path, directory.path());
    c.force_thread_mirror();
    barrier(&storage);
    assert_eq!(
        projected(&config, "thread").map(|row| (row.0, row.1)),
        Some((true, Some(1)))
    );
}

#[test]
fn a_file_of_the_older_form_met_after_start_up_is_not_converted_or_published() {
    // Given: a mirroring window on an archived family, and a program of the older form
    // that overwrites the shared file with per-thread flags (everything unarchived).
    let directory = tempfile::tempdir().unwrap();
    let (mut a, _, path) = two_windows(directory.path());
    a.toggle_archive(ThreadId::new("thread")).unwrap();
    drop(a);
    let (mut b, storage, config) = mirroring(&path, directory.path());
    b.force_thread_mirror();
    barrier(&storage);
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["version"] = 1.into();
    json.as_object_mut().unwrap().remove("families");
    json["threads"][0]["archived"] = false.into();
    json["threads"][0]["pinned"] = true.into();
    std::fs::write(&path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

    // When: B takes the file in and saves.
    b.import_shared_sidebar();
    b.save_sidebar();
    b.force_thread_mirror();
    barrier(&storage);

    // Then: no conversion made a state from it: B's archive stands, nothing was pinned
    // by it, and the database did not move.
    assert_eq!(series(&b, "thread"), (true, series(&b, "thread").1, 1));
    assert_eq!(pin_series(b.sidebar(), "thread"), (false, 0));
    assert_eq!(
        projected(&config, "thread").map(|row| (row.0, row.1, row.2)),
        Some((true, Some(1), false))
    );
}
