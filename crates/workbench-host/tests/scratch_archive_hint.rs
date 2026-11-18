//! Archiving a family tells the scratch owner, and only after the archive is
//! saved: every member, again on a repeated request, whatever happens to the
//! attachments afterwards, and never when the save did not happen. The hint
//! carries thread IDs only; collecting the scratch is the owner's business
//! (`bootstrap::scratch_archive_tests` covers it through the production host).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use workbench_host::areas::workspace::{ScratchHint, WorkspaceArea};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{CommandResult, ThreadTarget};
use workbench_protocol::ids::ThreadId as WireThread;
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::PageRequest;
use workspace_state::{
    ForkPoint, LineageKind, ProjectId, SidebarDocument, SidebarStore, ThreadId, ThreadLineage,
    ThreadRecord,
};

/// What the hint saw: the thread and whether the saved sidebar already said it
/// was archived, with the save lock free for anyone else to take.
type Seen = Arc<Mutex<Vec<(String, bool)>>>;

struct Fixture {
    base: TempDir,
    config: PathBuf,
    seen: Seen,
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

fn fixture(with_attachments: bool) -> Fixture {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("repo");
    let config = base.path().join("config");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let root = root.canonicalize().unwrap();
    let config = config.canonicalize().unwrap();
    let path = config.join("sidebar.json");
    let store = SidebarStore::new(path.clone());
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(ProjectId::new("p1"), "repo", &root)
        .unwrap();
    document
        .sidebar
        .threads
        .extend([thread("root"), fork_of("fork", "root"), thread("other")]);
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let runtime = with_attachments.then(|| {
        let runtime = common::runtime();
        runtime.set_thread_scratch(Arc::new(runtime::scratch::ThreadScratch::at(
            base.path().join("scratch"),
        )));
        runtime
    });
    let seen = Seen::default();
    let hint: ScratchHint = {
        let seen = seen.clone();
        let path = path.clone();
        Arc::new(move |thread: &str| {
            let store = SidebarStore::new(path.clone());
            // Taking the save lock proves the archive command released it first.
            drop(store.lock().unwrap());
            let archived = store
                .read_unlocked()
                .unwrap()
                .sidebar
                .resolve_states()
                .get(&ThreadId::new(thread))
                .is_some_and(|state| state.archived);
            seen.lock().unwrap().push((thread.to_owned(), archived));
        })
    };
    let area = WorkspaceArea::with_store(store, runtime, vec![config.clone()], None, |_| false)
        .with_scratch_hint(hint);
    Fixture {
        base,
        config,
        seen,
        area,
    }
}

fn wire(id: &str) -> WireThread {
    WireThread::new(id).unwrap()
}

impl Fixture {
    fn family_of(&self, id: &str) -> FamilyView {
        let WorkspaceReply::Families(page) = self
            .area
            .query(&WorkspaceQuery::ListFamilies(PageRequest::default()))
            .unwrap()
        else {
            panic!("not families")
        };
        let detail = match self
            .area
            .query(&WorkspaceQuery::GetThreadDetail(ThreadTarget {
                thread: wire(id),
            }))
            .unwrap()
        {
            WorkspaceReply::ThreadDetail(detail) => detail,
            other => panic!("{other:?}"),
        };
        page.items
            .into_iter()
            .find(|family| Some(&family.key) == detail.family.as_ref())
            .expect("the thread belongs to a family")
    }

    fn archive(&self, id: &str, archived: bool) -> WorkspaceResult {
        let family = self.family_of(id);
        self.archive_with(id, archived, &family)
    }

    fn archive_with(&self, id: &str, archived: bool, family: &FamilyView) -> WorkspaceResult {
        let reply = self
            .area
            .command(
                WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
                    thread: wire(id),
                    archived,
                    expected_family: FamilyIdentity {
                        key: family.key.clone(),
                        membership_epoch: family.membership_epoch,
                    },
                    expected_revision: family.archive_revision,
                }),
                &local_desktop(),
            )
            .unwrap();
        match *reply.result.expect("a workspace command carries a result") {
            CommandResult::Workspace(result) => result,
            other => panic!("{other:?}"),
        }
    }

    fn hinted(&self) -> Vec<(String, bool)> {
        let mut seen = std::mem::take(&mut *self.seen.lock().unwrap());
        seen.sort();
        seen
    }
}

fn archived(thread: &str) -> (String, bool) {
    (thread.to_owned(), true)
}

#[test]
fn every_family_member_is_hinted_after_the_archive_is_saved_and_its_lock_released() {
    let f = fixture(false);
    assert!(matches!(
        f.archive("fork", true),
        WorkspaceResult::Applied(_)
    ));
    // The unrelated thread is not part of the family.
    assert_eq!(f.hinted(), [archived("fork"), archived("root")]);
}

#[test]
fn a_repeated_archive_request_hints_again() {
    let f = fixture(false);
    assert!(matches!(
        f.archive("root", true),
        WorkspaceResult::Applied(_)
    ));
    f.hinted();
    assert!(matches!(
        f.archive("root", true),
        WorkspaceResult::Applied(_)
    ));
    assert_eq!(f.hinted(), [archived("fork"), archived("root")]);
}

#[test]
fn unarchiving_and_a_stale_request_send_no_hint() {
    let f = fixture(false);
    let stale = f.family_of("root");
    assert!(matches!(
        f.archive("root", true),
        WorkspaceResult::Applied(_)
    ));
    f.hinted();
    assert!(matches!(
        f.archive("root", false),
        WorkspaceResult::Applied(_)
    ));
    assert!(matches!(
        f.archive_with("root", true, &stale),
        WorkspaceResult::Rejected(WorkspaceRejection::Conflict { .. })
    ));
    assert!(f.hinted().is_empty());
}

#[test]
fn a_failed_save_sends_no_hint() {
    let f = fixture(false);
    let family = f.family_of("root");
    // The atomic write needs a new file in the directory, which a read-only
    // directory refuses.
    drop(
        SidebarStore::new(f.config.join("sidebar.json"))
            .lock()
            .unwrap(),
    );
    std::fs::set_permissions(&f.config, std::fs::Permissions::from_mode(0o555)).unwrap();
    let writable = std::fs::File::create(f.config.join("probe")).is_ok();
    let result = f.archive_with("root", true, &family);
    std::fs::set_permissions(&f.config, std::fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        // Running with privileges that ignore permissions: nothing to assert.
        return;
    }
    assert!(matches!(
        result,
        WorkspaceResult::Rejected(WorkspaceRejection::StorageFailed { .. })
    ));
    assert!(f.hinted().is_empty());
}

#[test]
fn busy_attachments_refuse_the_archive_before_any_hint() {
    let f = fixture(true);
    let root = f.area_attachment_root();
    let _held = root.try_lock_thread("fork").unwrap();
    assert!(matches!(
        f.archive("root", true),
        WorkspaceResult::Rejected(WorkspaceRejection::StorageFailed { .. })
    ));
    assert!(f.hinted().is_empty());
}

#[test]
fn scratch_hints_survive_a_failed_attachment_removal() {
    use rustix::fs::OFlags;
    use std::io::Write;
    let f = fixture(true);
    let root = f.area_attachment_root();
    let files = root.files("root").unwrap();
    files
        .file("old.txt", OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY)
        .unwrap()
        .write_all(b"old")
        .unwrap();
    std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = f.archive("root", true);
    std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    if std::fs::read_dir(files.path()).is_ok_and(|mut entries| entries.next().is_none()) {
        // Running with privileges that ignore permissions: removal succeeded.
        return;
    }
    assert!(matches!(
        result,
        WorkspaceResult::Rejected(WorkspaceRejection::StorageFailed { .. })
    ));
    // The archive was saved, so the owner is told although the response failed.
    assert_eq!(f.hinted(), [archived("fork"), archived("root")]);
}

impl Fixture {
    fn area_attachment_root(&self) -> sandbox::attachments::AttachmentRoot {
        let runtime = common::runtime();
        runtime.set_thread_scratch(Arc::new(runtime::scratch::ThreadScratch::at(
            self.base.path().join("scratch"),
        )));
        runtime.attachment_root().unwrap()
    }
}
