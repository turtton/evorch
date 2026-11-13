//! The retention view of the `workspace` area is computed by the core from the
//! sidebar and the database, with no client involved. These tests drive the
//! computation over a real database file and a sidebar written by the store.

use storage::thread_retention::{NANOS_PER_DAY, ThreadRetentionPolicy};
use storage::{Database, StorageConfig};
use workbench_host::areas::workspace::{RetentionSource, retention_entries};
use workbench_protocol::namespaces::workspace::{RetentionBlock, RetentionState};
use workspace_state::{ProjectId, SidebarDocument, ThreadId, ThreadRecord};

struct Fixture {
    _dir: tempfile::TempDir,
    database: Database,
    source: RetentionSource,
    document: SidebarDocument,
}

fn fixture(days: u32, threads: &[&str]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let owners = dir.path().join("owners");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&owners).unwrap();
    let sidebar_path = dir.path().join("sidebar.json");
    std::fs::write(&sidebar_path, b"{}").unwrap();
    let database = Database::open(&StorageConfig {
        db_path: dir.path().join("host.db"),
        ..StorageConfig::default()
    })
    .unwrap();
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(ProjectId::new("p1"), "p", &root)
        .unwrap();
    for id in threads {
        document.sidebar.threads.push(ThreadRecord::new(
            ThreadId::new(*id),
            ProjectId::new("p1"),
            *id,
        ));
    }
    document.sidebar.refresh_derived();
    Fixture {
        source: RetentionSource {
            ownership_root: owners,
            sidebar_path,
            policy: ThreadRetentionPolicy {
                archived_delete_after_days: days,
                ..ThreadRetentionPolicy::default()
            },
        },
        _dir: dir,
        database,
        document,
    }
}

fn states(f: &Fixture, now_days: i64) -> Vec<(String, RetentionState)> {
    retention_entries(
        &f.database,
        &f.document,
        &f.source,
        now_days * NANOS_PER_DAY,
    )
    .unwrap()
    .into_iter()
    .map(|entry| (entry.thread.as_str().to_owned(), entry.state))
    .collect()
}

#[test]
fn statuses_follow_the_family_state_of_the_sidebar_without_any_client() {
    let mut f = fixture(7, &["keep", "old", "pinned"]);
    let long_ago = 1_000;
    f.document
        .sidebar
        .set_family_archived(&ThreadId::new("old"), true, long_ago)
        .unwrap();
    f.document
        .sidebar
        .set_family_archived(&ThreadId::new("pinned"), true, long_ago)
        .unwrap();
    f.document
        .sidebar
        .set_family_pinned(&ThreadId::new("pinned"), true)
        .unwrap();
    let states = states(&f, 20_000);
    let of = |id: &str| &states.iter().find(|(thread, _)| thread == id).unwrap().1;
    assert_eq!(of("keep"), &RetentionState::NotArchived);
    assert!(
        !matches!(
            of("old"),
            RetentionState::NotArchived | RetentionState::Disabled
        ),
        "an archived thread older than the period is on its way out: {:?}",
        of("old")
    );
    assert_eq!(
        of("pinned"),
        &RetentionState::Blocked {
            block: RetentionBlock::Pinned
        }
    );
}

#[test]
fn a_zero_period_switches_deleting_off() {
    let mut f = fixture(0, &["old"]);
    f.document
        .sidebar
        .set_family_archived(&ThreadId::new("old"), true, 1_000)
        .unwrap();
    assert_eq!(
        states(&f, 20_000),
        vec![("old".to_owned(), RetentionState::Disabled)]
    );
}
