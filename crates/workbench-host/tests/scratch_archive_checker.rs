//! The checker the host and the native GUI inject into the owner decides from
//! the saved shared sidebar alone: archived collects, anything it cannot
//! establish keeps. These tests drive a real `OwnerHost` through
//! `scratch_archive::install`, the function both production paths call, and
//! wait on the archive worker's barrier.

use runtime::ownership::OwnerHost;
use sandbox::attachments::AttachmentRoot;
use sandbox::scratch::ScratchRoot;
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use workbench_host::scratch_archive::{install, sidebar_checker};
use workspace_state::{ProjectId, SidebarDocument, SidebarStore, ThreadId, ThreadRecord};

struct World {
    sockets: test_socket_dir::SocketDir,
    data: tempfile::TempDir,
    host: OwnerHost,
    sidebar: PathBuf,
}

fn record(id: &str, parent: Option<&str>) -> ThreadRecord {
    let mut record = ThreadRecord::new(ThreadId::new(id), ProjectId::new("p"), id);
    record.parent_thread_id = parent.map(ThreadId::new);
    record
}

fn world() -> World {
    let sockets = test_socket_dir::SocketDir::new();
    let data = tempfile::tempdir().unwrap();
    let host = OwnerHost::open(
        sockets.path(),
        Default::default(),
        Arc::new(event_bus::EventBus::new(32)),
    )
    .unwrap();
    let sidebar = data.path().join("sidebar.json");
    World {
        sockets,
        data,
        host,
        sidebar,
    }
}

impl World {
    fn save(&self, threads: Vec<ThreadRecord>, archived: &[&str]) {
        self.save_without(threads, archived, &[]);
    }

    /// Saves the family archived, then drops the `missing` records from the file so
    /// the remaining threads name parents that are not listed.
    fn save_without(&self, threads: Vec<ThreadRecord>, archived: &[&str], missing: &[&str]) {
        let mut document = SidebarDocument::default();
        document
            .sidebar
            .add_project(ProjectId::new("p"), "p", self.data.path())
            .unwrap();
        document.sidebar.threads.extend(threads);
        for id in archived {
            document
                .sidebar
                .set_family_archived(&ThreadId::new(*id), true, 1)
                .unwrap();
        }
        document
            .sidebar
            .threads
            .retain(|thread| !missing.iter().any(|id| thread.id == ThreadId::new(*id)));
        let store = SidebarStore::new(self.sidebar.clone());
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }

    /// A scratch directory with content, owned by this process.
    fn scratch(&self, thread: &str) -> PathBuf {
        self.host.start(thread).unwrap();
        let scratch = self.host.thread_scratch();
        let dir = scratch.thread_dir(thread);
        let name = dir.file_name().unwrap().to_str().unwrap();
        // Same identity-marked root `ThreadScratch::for_ownership_root` builds, so
        // the owner treats the parent as its own.
        let canonical = self.sockets.path().canonicalize().unwrap();
        let digest = format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()));
        let prepared = ScratchRoot::fixed(&digest[..8], &digest)
            .unwrap()
            .prepare(name)
            .unwrap();
        assert_eq!(prepared, dir);
        std::fs::write(dir.join("keep"), b"scratch").unwrap();
        dir
    }

    /// One archive pass over `thread`, finished (renamed or kept) on return.
    fn hint_and_wait(&self, thread: &str) {
        self.host.hint_scratch_archive(thread);
        self.host.scratch_archive_barrier().recv().unwrap();
    }
}

#[test]
fn a_saved_archived_thread_is_collected() {
    let w = world();
    w.save(vec![record("t", None)], &["t"]);
    install(&w.host, Some(w.sidebar.clone()));
    let dir = w.scratch("t");
    w.hint_and_wait("t");
    assert!(!dir.exists());
}

#[test]
fn a_saved_unarchived_thread_keeps_its_scratch_and_the_hint_is_dropped() {
    let w = world();
    w.save(vec![record("t", None)], &[]);
    install(&w.host, Some(w.sidebar.clone()));
    let dir = w.scratch("t");
    w.hint_and_wait("t");
    assert!(dir.join("keep").exists());
    // The hint is gone: archiving later without a new hint collects nothing.
    w.save(vec![record("t", None)], &["t"]);
    w.host.scratch_archive_barrier().recv().unwrap();
    assert!(dir.join("keep").exists());
}

#[test]
fn collection_does_not_wait_for_the_sidebar_save_lock() {
    let w = world();
    w.save(vec![record("t", None)], &["t"]);
    install(&w.host, Some(w.sidebar.clone()));
    let dir = w.scratch("t");
    let store = SidebarStore::new(w.sidebar.clone());
    let _held = store.lock().unwrap();
    w.hint_and_wait("t");
    assert!(
        !dir.exists(),
        "the pass finished while the save lock was held"
    );
}

#[test]
fn anything_the_checker_cannot_establish_keeps_the_scratch() {
    // No shared sidebar at all.
    let w = world();
    install(&w.host, None);
    let dir = w.scratch("t");
    w.hint_and_wait("t");
    assert!(dir.join("keep").exists());

    // The file is not there, so the thread is unknown.
    install(&w.host, Some(w.sidebar.clone()));
    w.hint_and_wait("t");
    assert!(dir.join("keep").exists());

    // The file cannot be parsed.
    std::fs::write(&w.sidebar, b"{ not json").unwrap();
    w.hint_and_wait("t");
    assert!(dir.join("keep").exists());

    // The thread is listed, but its parent is not: the family is unresolved,
    // even though the thread itself is marked archived.
    w.save_without(
        vec![record("gone", None), record("t", Some("gone"))],
        &["t"],
        &["gone"],
    );
    w.hint_and_wait("t");
    assert!(dir.join("keep").exists());

    // Once the family resolves, the retained hint collects.
    w.save(vec![record("t", None)], &["t"]);
    w.hint_and_wait("t");
    assert!(!dir.exists());
}

#[test]
fn the_checker_answers_from_the_resolved_family_state() {
    let w = world();
    w.save(
        vec![
            record("root", None),
            record("fork", Some("root")),
            record("lone", None),
        ],
        &["root", "fork"],
    );
    let check = sidebar_checker(Some(w.sidebar.clone()));
    assert!(check("root").unwrap());
    assert!(check("fork").unwrap());
    assert!(!check("lone").unwrap());
    assert_eq!(
        check("missing").unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(sidebar_checker(None)("root").is_err());
}

#[test]
fn collection_never_touches_attachments() {
    use rustix::fs::OFlags;
    use std::io::Write;
    let w = world();
    w.save(vec![record("t", None)], &["t"]);
    install(&w.host, Some(w.sidebar.clone()));
    let dir = w.scratch("t");
    let canonical = Path::new(w.sockets.path()).canonicalize().unwrap();
    let root = AttachmentRoot::for_scope(&canonical);
    let files = root.files("t").unwrap();
    files
        .file(
            "sentinel.txt",
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
        )
        .unwrap()
        .write_all(b"attachment")
        .unwrap();
    w.hint_and_wait("t");
    assert!(!dir.exists());
    assert_eq!(
        std::fs::read(files.path().join("sentinel.txt")).unwrap(),
        b"attachment"
    );
    // Test cleanup of the attachment root belongs to the test.
    let _lock = root.lock_thread("t").unwrap();
    root.remove_thread("t").unwrap();
}
