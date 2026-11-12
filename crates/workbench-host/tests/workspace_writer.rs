//! The core is the writer of the shared sidebar (PR 09): stale revisions,
//! tombstones, concurrent clients, a lock held by another process, a desktop
//! that still saves the file, a failed save and a thread that must not come
//! back. The entry is `WorkspaceArea::with_store`, over an explicit file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use workbench_host::areas::workspace::WorkspaceArea;
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{CommandReply, CommandResult, ThreadTarget};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::ids::{ProjectId as WireProject, RunId, ThreadId as WireThread};
use workbench_protocol::namespaces::WorkspaceQuery;
use workbench_protocol::namespaces::WorkspaceReply;
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::queries::PageRequest;
use workbench_protocol::wire::Revision;
use workspace_state::{
    ForkPoint, LineageKind, ProjectId, SidebarDocument, SidebarStore, ThreadId, ThreadLineage,
    ThreadRecord,
};

struct Fixture {
    _base: TempDir,
    root: PathBuf,
    config: PathBuf,
    store: SidebarStore,
    deleted: Arc<std::sync::Mutex<Vec<String>>>,
    area: WorkspaceArea,
}

fn thread(id: &str) -> ThreadRecord {
    ThreadRecord::new(ThreadId::new(id), ProjectId::new("p1"), id)
}

fn fork_of(id: &str, parent: &str) -> ThreadRecord {
    let mut record = thread(id);
    record.parent_thread_id = Some(ThreadId::new(parent));
    record.lineage = Some(ThreadLineage {
        kind: LineageKind::Fork,
        point: Some(ForkPoint {
            run_id: "run-1".into(),
            context_len: 2,
        }),
    });
    record
}

fn fixture(threads: Vec<ThreadRecord>) -> Fixture {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("repo");
    let config = base.path().join("config");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let root = root.canonicalize().unwrap();
    let config = config.canonicalize().unwrap();
    let store = SidebarStore::new(config.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(ProjectId::new("p1"), "repo", &root)
        .unwrap();
    document.sidebar.threads.extend(threads);
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let deleted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = deleted.clone();
    let area = WorkspaceArea::with_store(
        SidebarStore::new(config.join("sidebar.json")),
        None,
        vec![config.clone()],
        None,
        move |id| seen.lock().unwrap().iter().any(|d| d == id),
    );
    Fixture {
        _base: base,
        root,
        config,
        store,
        deleted,
        area,
    }
}

fn wire_thread(id: &str) -> WireThread {
    WireThread::new(id).unwrap()
}

fn wire_project() -> WireProject {
    WireProject::new("p1").unwrap()
}

fn agent() -> Provenance {
    Provenance::agent(RunId::new("run-agent").unwrap())
}

fn result_of(reply: CommandReply) -> WorkspaceResult {
    match *reply.result.expect("a workspace command carries a result") {
        CommandResult::Workspace(result) => result,
        other => panic!("not a workspace result: {other:?}"),
    }
}

impl Fixture {
    fn send(&self, command: WorkspaceCommand) -> WorkspaceResult {
        self.send_as(command, &local_desktop())
    }

    fn send_as(&self, command: WorkspaceCommand, who: &Provenance) -> WorkspaceResult {
        result_of(self.area.command(command, who).unwrap())
    }

    fn detail(&self, id: &str) -> ThreadDetail {
        match self
            .area
            .query(&WorkspaceQuery::GetThreadDetail(ThreadTarget {
                thread: wire_thread(id),
            }))
            .unwrap()
        {
            WorkspaceReply::ThreadDetail(detail) => *detail,
            other => panic!("{other:?}"),
        }
    }

    fn access(&self) -> ProjectAccessView {
        match self
            .area
            .query(&WorkspaceQuery::GetProjectAccess(ProjectTarget {
                project: wire_project(),
            }))
            .unwrap()
        {
            WorkspaceReply::ProjectAccess(view) => *view,
            other => panic!("{other:?}"),
        }
    }

    fn families(&self) -> Vec<FamilyView> {
        match self
            .area
            .query(&WorkspaceQuery::ListFamilies(PageRequest::default()))
            .unwrap()
        {
            WorkspaceReply::Families(page) => page.items,
            other => panic!("{other:?}"),
        }
    }

    fn rename(&self, id: &str, title: &str, expected: Revision) -> WorkspaceResult {
        self.send(WorkspaceCommand::RenameThread(RenameThread {
            thread: wire_thread(id),
            title: title.into(),
            expected_revision: expected,
        }))
    }

    fn on_disk(&self) -> SidebarDocument {
        self.store.read_unlocked().unwrap()
    }

    fn delete(&self, id: &str) {
        self.deleted.lock().unwrap().push(id.to_owned());
    }
}

fn applied(result: WorkspaceResult) -> Applied {
    match result {
        WorkspaceResult::Applied(applied) => applied,
        WorkspaceResult::Rejected(rejection) => panic!("rejected: {rejection:?}"),
    }
}

fn rejected(result: WorkspaceResult) -> WorkspaceRejection {
    match result {
        WorkspaceResult::Rejected(rejection) => rejection,
        WorkspaceResult::Applied(applied) => panic!("applied: {applied:?}"),
    }
}

fn title_on_disk(fixture: &Fixture, id: &str) -> Option<String> {
    fixture
        .on_disk()
        .sidebar
        .threads
        .iter()
        .find(|t| t.id.to_string() == id)
        .map(|t| t.title.clone())
}

#[test]
fn a_stale_revision_is_a_conflict_and_changes_nothing() {
    let f = fixture(vec![thread("t1")]);
    let first = f.detail("t1").revision;
    let renamed = applied(f.rename("t1", "first", first));
    assert!(renamed.changed);
    assert_ne!(renamed.revision, first);
    let before = std::fs::read(f.store.path()).unwrap();

    let stale = rejected(f.rename("t1", "second", first));
    assert_eq!(
        stale,
        WorkspaceRejection::Conflict {
            expected: first,
            actual: renamed.revision
        }
    );
    assert_eq!(std::fs::read(f.store.path()).unwrap(), before);
    assert_eq!(f.detail("t1").title, "first");
    // Read again, decide again: the fresh revision applies.
    applied(f.rename("t1", "second", renamed.revision));
    assert_eq!(f.detail("t1").title, "second");
}

#[test]
fn two_clients_with_the_same_revision_cannot_overwrite_each_other() {
    let f = fixture(vec![thread("t1")]);
    let seen = f.detail("t1").revision;
    // Client A and client B both read `seen`; whichever is first wins.
    applied(f.rename("t1", "from A", seen));
    assert!(matches!(
        rejected(f.rename("t1", "from B", seen)),
        WorkspaceRejection::Conflict { .. }
    ));
    assert_eq!(title_on_disk(&f, "t1").as_deref(), Some("from A"));
}

#[test]
fn simultaneous_commands_from_several_clients_all_land_or_are_told_to_retry() {
    let ids: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
    let f = fixture(ids.iter().map(|id| thread(id)).collect());
    let revisions: Vec<Revision> = ids.iter().map(|id| f.detail(id).revision).collect();
    let area = &f.area;
    std::thread::scope(|scope| {
        for (id, revision) in ids.iter().zip(&revisions) {
            scope.spawn(move || {
                loop {
                    let result = result_of(
                        area.command(
                            WorkspaceCommand::RenameThread(RenameThread {
                                thread: wire_thread(id),
                                title: format!("renamed {id}"),
                                expected_revision: *revision,
                            }),
                            &local_desktop(),
                        )
                        .unwrap(),
                    );
                    match result {
                        WorkspaceResult::Applied(_) => break,
                        // The lock is held by a sibling: the documented answer
                        // is to resend the same request.
                        WorkspaceResult::Rejected(WorkspaceRejection::Busy) => {
                            std::thread::yield_now();
                        }
                        WorkspaceResult::Rejected(other) => panic!("{other:?}"),
                    }
                }
            });
        }
    });
    for id in &ids {
        assert_eq!(
            title_on_disk(&f, id).as_deref(),
            Some(format!("renamed {id}").as_str()),
            "no rename was lost"
        );
    }
}

#[test]
fn a_lock_held_elsewhere_is_busy_and_the_file_is_untouched() {
    let f = fixture(vec![thread("t1")]);
    let revision = f.detail("t1").revision;
    let before = std::fs::read(f.store.path()).unwrap();
    let held = f.store.lock().unwrap();
    assert_eq!(
        f.rename("t1", "x", revision),
        WorkspaceResult::Rejected(WorkspaceRejection::Busy)
    );
    drop(held);
    assert_eq!(std::fs::read(f.store.path()).unwrap(), before);
    applied(f.rename("t1", "x", revision));
}

#[test]
fn a_deleted_thread_is_tombstoned_and_never_comes_back() {
    let f = fixture(vec![thread("t1"), thread("t2")]);
    let r1 = f.detail("t1").revision;
    let r2 = f.detail("t2").revision;
    f.delete("t1");

    assert_eq!(
        f.rename("t1", "x", r1),
        WorkspaceResult::Rejected(WorkspaceRejection::Tombstoned)
    );
    assert_eq!(
        f.send(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
            thread: wire_thread("t1"),
            archived: true,
            expected_revision: Revision(0),
        })),
        WorkspaceResult::Rejected(WorkspaceRejection::Tombstoned)
    );
    // Queries do not show it either.
    assert!(
        f.families()
            .iter()
            .all(|family| family.key.as_str() != "t1")
    );
    assert!(
        f.area
            .query(&WorkspaceQuery::GetThreadDetail(ThreadTarget {
                thread: wire_thread("t1"),
            }))
            .is_err()
    );
    // The next write by anyone removes the saved copy.
    applied(f.rename("t2", "kept", r2));
    let on_disk = f.on_disk();
    assert!(
        on_disk
            .sidebar
            .threads
            .iter()
            .all(|t| t.id.to_string() != "t1")
    );
    assert!(
        on_disk
            .sidebar
            .threads
            .iter()
            .any(|t| t.id.to_string() == "t2")
    );
}

#[test]
fn a_legacy_desktop_save_that_brings_a_deleted_thread_back_is_pruned_by_the_next_write() {
    let f = fixture(vec![thread("t1"), thread("t2")]);
    f.delete("t1");
    // A desktop holding an old in-memory copy saves it, thread t1 included,
    // together with fields only the desktop interprets.
    let mut legacy = f.on_disk();
    legacy.rest.insert(
        "user_messages".into(),
        serde_json::json!([{"id": "m1", "thread_id": "t1"}, {"id": "m2", "thread_id": "t2"}]),
    );
    {
        let lock = f.store.lock().unwrap();
        f.store.write(&lock, &legacy).unwrap();
    }
    let r2 = f.detail("t2").revision;
    applied(f.rename("t2", "kept", r2));
    let on_disk = f.on_disk();
    assert!(
        on_disk
            .sidebar
            .threads
            .iter()
            .all(|t| t.id.to_string() != "t1")
    );
    assert_eq!(
        on_disk.rest["user_messages"],
        serde_json::json!([{"id": "m2", "thread_id": "t2"}]),
        "the tombstoned thread's messages go, the rest survives"
    );
}

#[test]
fn a_command_is_applied_to_what_a_legacy_save_just_wrote() {
    let f = fixture(vec![thread("t1"), thread("t2")]);
    let r1 = f.detail("t1").revision;
    // Between the client's read and its command the desktop saves a rename of
    // another thread. The core re-reads under the lock, so neither is lost.
    let mut legacy = f.on_disk();
    legacy
        .sidebar
        .threads
        .iter_mut()
        .find(|t| t.id.to_string() == "t2")
        .unwrap()
        .title = "desktop rename".into();
    {
        let lock = f.store.lock().unwrap();
        f.store.write(&lock, &legacy).unwrap();
    }
    applied(f.rename("t1", "core rename", r1));
    assert_eq!(title_on_disk(&f, "t1").as_deref(), Some("core rename"));
    assert_eq!(title_on_disk(&f, "t2").as_deref(), Some("desktop rename"));
}

#[test]
fn an_unreadable_sidebar_is_reported_and_left_as_it_was() {
    let f = fixture(vec![thread("t1")]);
    let revision = f.detail("t1").revision;
    std::fs::write(f.store.path(), b"{ not json").unwrap();
    assert!(matches!(
        rejected(f.rename("t1", "x", revision)),
        WorkspaceRejection::StorageFailed { .. }
    ));
    assert_eq!(std::fs::read(f.store.path()).unwrap(), b"{ not json");
}

#[cfg(unix)]
#[test]
fn a_failed_save_changes_nothing_and_publishes_no_new_state() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture(vec![thread("t1")]);
    let revision = f.detail("t1").revision;
    let before = std::fs::read(f.store.path()).unwrap();
    // The lock file exists already; the atomic write needs a new file in the
    // directory, which a read-only directory refuses.
    drop(f.store.lock().unwrap());
    std::fs::set_permissions(&f.config, std::fs::Permissions::from_mode(0o555)).unwrap();
    let probe = f.config.join("probe");
    let writable = std::fs::File::create(&probe).is_ok();
    let result = f.rename("t1", "lost", revision);
    std::fs::set_permissions(&f.config, std::fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        // Running with privileges that ignore permissions: nothing to assert.
        let _ = std::fs::remove_file(&probe);
        return;
    }
    assert!(matches!(
        rejected(result),
        WorkspaceRejection::StorageFailed { .. }
    ));
    assert_eq!(std::fs::read(f.store.path()).unwrap(), before);
    assert_eq!(f.detail("t1").title, "t1");
    applied(f.rename("t1", "after", revision));
}

#[test]
fn only_a_human_may_change_trust_or_reachable_directories() {
    let f = fixture(vec![thread("t1")]);
    let revision = f.access().revision;
    let command = WorkspaceCommand::SetProjectTrust(SetProjectTrust {
        project: wire_project(),
        trust: TrustLevel::Approved,
        expected_revision: revision,
    });
    assert!(matches!(
        rejected(f.send_as(command.clone(), &agent())),
        WorkspaceRejection::PermissionDenied { .. }
    ));
    assert_eq!(f.access().trust, TrustLevel::Unapproved);
    let result = applied(f.send_as(command, &local_desktop()));
    assert!(result.changed);
    assert_eq!(f.access().trust, TrustLevel::Approved);
    // Setting the same value again reports no change.
    let again = applied(f.send(WorkspaceCommand::SetProjectTrust(SetProjectTrust {
        project: wire_project(),
        trust: TrustLevel::Approved,
        expected_revision: result.revision,
    })));
    assert!(!again.changed);
}

fn add_directory(f: &Fixture, path: &Path) -> WorkspaceResult {
    f.send(WorkspaceCommand::UpdateAllowedDirectory(
        UpdateAllowedDirectory {
            project: wire_project(),
            action: AllowedDirectoryAction::Add {
                path: path.to_string_lossy().into_owned(),
                trust: TrustLevel::Approved,
            },
            expected_revision: f.access().revision,
        },
    ))
}

#[test]
fn directories_are_validated_by_the_core() {
    let f = fixture(vec![thread("t1")]);
    let extra = f.root.parent().unwrap().join("shared");
    std::fs::create_dir_all(&extra).unwrap();
    // Relative, missing, and the core's own configuration are refused.
    assert!(matches!(
        rejected(add_directory(&f, Path::new("relative/dir"))),
        WorkspaceRejection::InvalidPath { .. }
    ));
    assert!(matches!(
        rejected(add_directory(&f, &f.root.parent().unwrap().join("missing"))),
        WorkspaceRejection::InvalidPath { .. }
    ));
    assert!(matches!(
        rejected(add_directory(&f, &f.config)),
        WorkspaceRejection::InvalidPath { .. }
    ));
    // A symlink to the configuration is judged by where it leads.
    #[cfg(unix)]
    {
        let link = f.root.parent().unwrap().join("link");
        std::os::unix::fs::symlink(&f.config, &link).unwrap();
        assert!(matches!(
            rejected(add_directory(&f, &link)),
            WorkspaceRejection::InvalidPath { .. }
        ));
    }
    assert!(f.access().allowed_directories.is_empty());
    // A real directory is accepted and is live.
    assert!(applied(add_directory(&f, &extra)).changed);
    let view = f.access();
    assert_eq!(view.allowed_directories.len(), 1);
    assert!(view.allowed_directories[0].live);
    // Once it disappears it stays declared but is not a live permission.
    std::fs::remove_dir_all(&extra).unwrap();
    assert!(!f.access().allowed_directories[0].live);
}

#[test]
fn extra_read_directories_are_validated_and_conflict_checked() {
    let f = fixture(vec![thread("t1")]);
    let revision = f.detail("t1").revision;
    let set = |dirs: Vec<String>, expected| {
        f.send(WorkspaceCommand::SetThreadExtraReadDirs(
            SetThreadExtraReadDirs {
                thread: wire_thread("t1"),
                dirs,
                expected_revision: expected,
            },
        ))
    };
    assert!(matches!(
        rejected(set(vec![f.config.to_string_lossy().into_owned()], revision)),
        WorkspaceRejection::InvalidPath { .. }
    ));
    let extra = f.root.parent().unwrap().join("notes");
    std::fs::create_dir_all(&extra).unwrap();
    let done = applied(set(vec![extra.to_string_lossy().into_owned()], revision));
    assert!(matches!(
        rejected(set(Vec::new(), revision)),
        WorkspaceRejection::Conflict { .. }
    ));
    assert_eq!(f.detail("t1").extra_read_dirs.len(), 1);
    assert_eq!(f.detail("t1").revision, done.revision);
}

#[test]
fn archive_and_pin_apply_to_the_whole_family_and_are_conflict_checked() {
    let f = fixture(vec![
        thread("root"),
        fork_of("fork", "root"),
        thread("other"),
    ]);
    let families = f.families();
    let family = families
        .iter()
        .find(|family| family.members == 2)
        .expect("root and its fork form one family");
    let archived = applied(
        f.send(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
            thread: wire_thread("fork"),
            archived: true,
            expected_revision: family.archive_revision,
        })),
    );
    assert!(archived.changed);
    assert!(f.detail("root").archived && f.detail("fork").archived);
    assert!(!f.detail("other").archived);
    // The same counter again is stale.
    assert!(matches!(
        rejected(
            f.send(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
                thread: wire_thread("root"),
                archived: false,
                expected_revision: family.archive_revision,
            }))
        ),
        WorkspaceRejection::Conflict { .. }
    ));
    let pinned = applied(f.send(WorkspaceCommand::SetFamilyPinned(SetFamilyPinned {
        thread: wire_thread("root"),
        pinned: true,
        expected_revision: family.pin_revision,
    })));
    assert!(pinned.changed);
    assert!(f.detail("root").pinned && f.detail("fork").pinned);
    assert!(f.on_disk().sidebar.threads.len() == 3);
}

#[test]
fn a_registered_project_is_canonical_and_a_duplicate_is_refused() {
    let f = fixture(Vec::new());
    let other = f.root.parent().unwrap().join("second");
    std::fs::create_dir_all(&other).unwrap();
    let register = |path: &Path| {
        f.send(WorkspaceCommand::RegisterProject(RegisterProject {
            name: "second".into(),
            path: path.to_string_lossy().into_owned(),
        }))
    };
    let done = applied(register(&other));
    assert!(done.project.is_some());
    assert!(matches!(
        rejected(register(&other)),
        WorkspaceRejection::Invalid { .. }
    ));
    assert!(matches!(
        rejected(register(&f.config)),
        WorkspaceRejection::InvalidPath { .. }
    ));
    assert!(matches!(
        rejected(register(Path::new("relative"))),
        WorkspaceRejection::InvalidPath { .. }
    ));
    let saved = f.on_disk();
    assert_eq!(saved.sidebar.projects.len(), 2);
}

#[test]
fn a_command_that_changes_nothing_leaves_the_file_byte_identical() {
    let f = fixture(vec![thread("t1")]);
    let revision = f.detail("t1").revision;
    let before = std::fs::read(f.store.path()).unwrap();
    let same = applied(f.rename("t1", "t1", revision));
    assert!(!same.changed);
    assert_eq!(std::fs::read(f.store.path()).unwrap(), before);
}

#[test]
fn archive_and_pin_of_a_family_are_projected_into_the_threads_table() {
    let mut f = fixture(vec![
        thread("root"),
        fork_of("fork", "root"),
        thread("other"),
    ]);
    let db_path = f.config.join("host.db");
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: db_path.clone(),
        ..storage::StorageConfig::default()
    })
    .unwrap();
    f.area = WorkspaceArea::with_store(
        SidebarStore::new(f.config.join("sidebar.json")),
        None,
        vec![f.config.clone()],
        None,
        |_| false,
    )
    .with_mirror(storage.handle());
    let family = f
        .families()
        .into_iter()
        .find(|family| family.members == 2)
        .unwrap();
    applied(
        f.send(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
            thread: wire_thread("fork"),
            archived: true,
            expected_revision: family.archive_revision,
        })),
    );
    applied(f.send(WorkspaceCommand::SetFamilyPinned(SetFamilyPinned {
        thread: wire_thread("root"),
        pinned: true,
        expected_revision: family.pin_revision,
    })));
    storage.close();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let row = |id: &str| -> Option<(bool, bool)> {
        conn.query_row(
            "SELECT archived, pinned FROM threads WHERE thread_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
    };
    assert_eq!(row("root"), Some((true, true)));
    assert_eq!(row("fork"), Some((true, true)));
    assert_eq!(
        row("other"),
        None,
        "a thread outside the family is untouched"
    );
}
