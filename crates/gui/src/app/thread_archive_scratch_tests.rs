//! Archiving in the native window tells the scratch owner, through the same
//! wiring function the binary calls, and collection does not depend on what
//! happens to the attachments afterwards.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use runtime::ownership::OwnerHost;
use sandbox::attachments::AttachmentRoot;
use sandbox::scratch::ScratchRoot;
use sha2::{Digest, Sha256};
use workspace_ui::ThreadId;

use super::WorkbenchState;
use super::delivery_tests::state;
use crate::fixture::DemoSource;
use crate::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};

struct Attachments(AttachmentRoot);

impl CommandSink for Attachments {
    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        Vec::new()
    }

    fn attachment_archive_guard(
        &self,
        threads: &[String],
    ) -> Result<Option<runtime::attachments::AttachmentArchive>, String> {
        runtime::attachments::AttachmentArchive::lock(self.0.clone(), threads.iter().cloned())
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

struct World {
    _sockets: test_socket_dir::SocketDir,
    dir: tempfile::TempDir,
    owner: Arc<OwnerHost>,
    state: WorkbenchState<DemoSource>,
}

fn world() -> World {
    let sockets = test_socket_dir::SocketDir::new();
    let dir = tempfile::tempdir().unwrap();
    let sidebar = dir.path().join("sidebar.json");
    let owner = Arc::new(
        OwnerHost::open(
            sockets.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(32)),
        )
        .unwrap(),
    );
    // No explicit install: `state(..)` sets the sidebar path and `with_ownership`
    // wires the checker, exactly as the production builder does.
    let state = state(&sidebar)
        .with_ownership(owner.clone())
        .with_command_sink(Box::new(Attachments(AttachmentRoot::at(
            dir.path().join("attachments"),
        ))));
    World {
        _sockets: sockets,
        dir,
        owner,
        state,
    }
}

impl World {
    /// A scratch directory with content under the owner's identity-marked root.
    fn scratch(&self, thread: &str) -> PathBuf {
        self.owner.start(thread).unwrap();
        let dir = self.owner.thread_scratch().thread_dir(thread);
        let digest = format!(
            "{:x}",
            Sha256::digest(
                self._sockets
                    .path()
                    .canonicalize()
                    .unwrap()
                    .as_os_str()
                    .as_bytes()
            )
        );
        let name = dir.file_name().unwrap().to_str().unwrap();
        let prepared = ScratchRoot::fixed(&digest[..8], &digest)
            .unwrap()
            .prepare(name)
            .unwrap();
        assert_eq!(prepared, dir);
        std::fs::write(dir.join("keep"), b"scratch").unwrap();
        dir
    }

    fn attachments(&self) -> AttachmentRoot {
        AttachmentRoot::at(self.dir.path().join("attachments"))
    }
}

fn is_root_like(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

#[test]
fn archiving_in_the_window_collects_the_scratch_through_the_production_wiring() {
    let mut w = world();
    let dir = w.scratch("thread");
    w.state.toggle_archive(ThreadId::new("thread")).unwrap();
    w.owner.scratch_archive_barrier().recv().unwrap();
    assert!(!dir.exists());
}

#[test]
fn restoring_a_thread_sends_no_hint_and_keeps_the_scratch() {
    let mut w = world();
    w.state.toggle_archive(ThreadId::new("thread")).unwrap();
    w.owner.scratch_archive_barrier().recv().unwrap();
    let dir = w.scratch("thread");
    w.state.toggle_archive(ThreadId::new("thread")).unwrap();
    // Nothing was hinted for the restore, so the next archive pass is the one
    // an explicit hint starts: it finds the thread unarchived and keeps it.
    w.owner.hint_scratch_archive("thread");
    w.owner.scratch_archive_barrier().recv().unwrap();
    assert!(dir.join("keep").exists());
}

#[test]
fn a_failed_attachment_removal_still_lets_the_scratch_be_collected() {
    use rustix::fs::OFlags;
    use std::io::Write;
    let mut w = world();
    let dir = w.scratch("thread");
    let files = w.attachments().files("thread").unwrap();
    files
        .file("old.txt", OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY)
        .unwrap()
        .write_all(b"old")
        .unwrap();
    std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = w.state.toggle_archive(ThreadId::new("thread"));
    std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    if is_root_like(files.path()) {
        // Privileges that ignore permissions: removal succeeded, nothing to inject.
        return;
    }
    assert!(result.is_err());
    w.owner.scratch_archive_barrier().recv().unwrap();
    assert!(!dir.exists());
}
