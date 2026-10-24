//! The sidebar's thread definitions are mirrored into the storage `threads`
//! table as they change (ADR 0041 §A-3).

use std::path::Path;
use std::time::Duration;

use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use storage::threads::{ThreadLineageKind, ThreadRow, ThreadSource, ThreadUpsert};
use storage::{Database, Storage, StorageConfig};

/// Steps frames until the mirrored row of `thread_id` satisfies `ready`. The
/// mirror syncs once a second; a hang is reported by the nextest watchdog
/// rather than a test deadline.
fn step_until_row(
    harness: &mut HeadlessWorkbench<DemoSource>,
    db_path: &Path,
    thread_id: &str,
    ready: impl Fn(&ThreadRow) -> bool,
) -> ThreadRow {
    loop {
        harness.step();
        let row = Database::open_read_only(db_path)
            .ok()
            .and_then(|database| database.thread(thread_id).ok().flatten());
        if let Some(row) = row.filter(|row| ready(row)) {
            return row;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn added_renamed_forked_and_archived_threads_follow_into_storage() {
    // Given: a workbench mirroring its sidebar into a fresh events database.
    let directory = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("workbench")
            .with_thread_mirror(storage.handle());
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 800.0]);
    let canonical_root = std::fs::canonicalize(project.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // When: a project is added and a thread is created in it.
    let project_id = harness.state_mut().add_project(project.path()).unwrap();
    harness
        .state_mut()
        .select_project(project_id.clone())
        .unwrap();
    let thread = harness.state_mut().create_thread("Alpha").unwrap();
    let thread_id = thread.to_string();

    // Then: its row carries the host's definition and the runtime's canonical root.
    let row = step_until_row(&mut harness, &config.db_path, &thread_id, |_| true);
    assert_eq!(row.title.as_deref(), Some("Alpha"));
    assert_eq!(
        row.project_id.as_deref(),
        Some(project_id.to_string().as_str())
    );
    assert_eq!(row.project_root.as_deref(), Some(canonical_root.as_str()));
    assert_eq!(row.source, ThreadSource::Host);
    assert!(!row.archived);
    assert!(row.created_at_ns > 0);
    assert_eq!(row.lineage_kind, None);

    // When: the thread is renamed.
    harness
        .state_mut()
        .rename_thread(thread.clone(), "Beta".into())
        .unwrap();

    // Then: the row follows on a later sync.
    step_until_row(&mut harness, &config.db_path, &thread_id, |row| {
        row.title.as_deref() == Some("Beta")
    });

    // When: the thread is forked.
    let fork = harness.state_mut().fork_thread(thread.clone()).unwrap();
    let fork_id = fork.to_string();

    // Then: the fork is mirrored as its child.
    let row = step_until_row(&mut harness, &config.db_path, &fork_id, |_| true);
    assert_eq!(row.parent_thread_id.as_deref(), Some(thread_id.as_str()));
    assert_eq!(row.lineage_kind, Some(ThreadLineageKind::Fork));
    assert_eq!(row.project_root.as_deref(), Some(canonical_root.as_str()));

    // When: the thread is archived, which archives its family.
    harness.state_mut().toggle_archive(thread.clone()).unwrap();

    // Then: both rows are archived and keep their other values.
    let row = step_until_row(&mut harness, &config.db_path, &thread_id, |row| {
        row.archived
    });
    assert_eq!(row.title.as_deref(), Some("Beta"));
    assert_eq!(row.source, ThreadSource::Host);
    step_until_row(&mut harness, &config.db_path, &fork_id, |row| row.archived);

    // When / Then: restoring the family clears the flag again.
    harness.state_mut().toggle_archive(thread).unwrap();
    step_until_row(&mut harness, &config.db_path, &thread_id, |row| {
        !row.archived
    });
    step_until_row(&mut harness, &config.db_path, &fork_id, |row| !row.archived);
    drop(harness);
    storage.close();
}

#[cfg(unix)]
#[test]
fn restored_sidebar_is_mirrored_under_canonical_roots_beside_runtime_values() {
    // Given: a sidebar.json that recorded its project through a symlink, and a
    // row the runtime wrote for its thread before the GUI bound a root run.
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let canonical_root = std::fs::canonicalize(&real)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let project_id = workspace_ui::ProjectId::new("demo");
    let mut sidebar = workspace_ui::SidebarState::default();
    sidebar.projects.push(workspace_ui::ProjectRecord {
        id: project_id.clone(),
        name: "demo".into(),
        repo_root: link,
        allowed_directories: Vec::new(),
        trust: workspace_ui::TrustState::Approved,
    });
    sidebar
        .create_thread(
            workspace_ui::ThreadId::new("thread-1"),
            project_id,
            "Fix login flow",
        )
        .unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .upsert_thread_runtime(&ThreadUpsert {
            thread_id: "thread-1".into(),
            project_root: Some(canonical_root.clone()),
            title: Some("Draft".into()),
            root_run_id: Some("run-7".into()),
            chat_role: Some("Worker".into()),
            created_at_ns: 5,
            updated_at_ns: 5,
            ..ThreadUpsert::default()
        })
        .unwrap();

    // When: the workbench restores that sidebar with the mirror attached.
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("workbench")
            .with_sidebar(sidebar)
            .with_thread_mirror(storage.handle());
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 800.0]);

    // Then: the host takes the row over with the symlink's canonical target, and
    // values the sidebar does not know yet are kept.
    let row = step_until_row(&mut harness, &config.db_path, "thread-1", |row| {
        row.source == ThreadSource::Host
    });
    assert_eq!(row.project_root.as_deref(), Some(canonical_root.as_str()));
    assert_eq!(row.project_id.as_deref(), Some("demo"));
    assert_eq!(row.title.as_deref(), Some("Fix login flow"));
    assert_eq!(row.root_run_id.as_deref(), Some("run-7"));
    assert_eq!(row.chat_role.as_deref(), Some("Worker"));
    assert!(
        row.created_at_ns > 5,
        "the host's creation time replaces it"
    );
    drop(harness);
    storage.close();
}
