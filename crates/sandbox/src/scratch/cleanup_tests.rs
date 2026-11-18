use super::*;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::{
        Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
};
struct Service(CleanupService, mpsc::Receiver<TestEvent>);
impl Service {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let mut s = CleanupService::new(Duration::ZERO);
        s.events = Some(tx);
        Self(s, rx)
    }
    fn pass(&self) {
        loop {
            match self.1.recv().unwrap() {
                TestEvent::Pass => break,
                TestEvent::Scan(path) => assert!(path.is_absolute()),
                TestEvent::Throttle => {}
                TestEvent::Stopped => panic!("stopped before pass"),
            }
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.0.stop().unwrap();
    }
}
fn root() -> (tempfile::TempDir, ScratchRoot) {
    let temp = tempfile::tempdir().unwrap();
    let scratch = ScratchRoot::at(temp.path().join("scratch")).identified("test-identity");
    scratch.prepare("live").unwrap();
    (temp, scratch)
}
fn trash(root: &ScratchRoot, name: &str) -> PathBuf {
    let path = root.parent.join(format!(".trash-{name}"));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("file"), "scratch").unwrap();
    path
}
#[test]
fn inventory_not_queue_retains_more_than_64_and_existing_trash() {
    let (_temp, root) = root();
    let s = Service::new();
    s.0.stop().unwrap();
    for i in 0..130 {
        trash(&root, &i.to_string());
    }
    s.0.wake_root(&root).unwrap();
    s.0.wake_root(&root).unwrap();
    s.0.resume().unwrap();
    s.pass();
    assert_eq!(
        fs::read_dir(&root.parent)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().as_encoded_bytes().starts_with(b".trash-"))
            .count(),
        0
    );
    assert!(root.dir("live").unwrap().exists());
}
#[test]
fn stop_mid_tree_join_and_restart_keeps_inventory() {
    let (_temp, root) = root();
    let path = trash(&root, "large");
    for i in 0..600 {
        fs::write(path.join(i.to_string()), "data").unwrap();
    }
    let mut s = Service::new();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let first = Arc::new(AtomicBool::new(true));
    s.0.step = Some(Arc::new({
        let entered = entered.clone();
        let release = release.clone();
        move || {
            if first.swap(false, Ordering::SeqCst) {
                entered.wait();
                release.wait();
            }
        }
    }));
    s.0.wake_root(&root).unwrap();
    entered.wait();
    // Set the same stop signal used by stop(); the worker is deterministically parked.
    lock(&s.0.shared.work).stop = true;
    s.0.shared.changed.notify_all();
    release.wait();
    s.0.stop().unwrap();
    assert!(path.exists());
    while !matches!(s.1.recv().unwrap(), TestEvent::Stopped) {}
    s.0.resume().unwrap();
    s.pass();
    assert!(!path.exists());
}
#[test]
fn rename_wake_after_dirty_clear_is_not_lost() {
    let (_temp, root) = root();
    let path = trash(&root, "first");
    let mut s = Service::new();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let first = Arc::new(AtomicBool::new(true));
    s.0.step = Some(Arc::new({
        let entered = entered.clone();
        let release = release.clone();
        move || {
            if first.swap(false, Ordering::SeqCst) {
                entered.wait();
                release.wait();
            }
        }
    }));
    s.0.wake_root(&root).unwrap();
    entered.wait();
    let later = trash(&root, "later");
    s.0.wake_root(&root).unwrap();
    release.wait();
    s.pass();
    s.pass();
    assert!(!path.exists());
    assert!(!later.exists());
}
#[test]
fn private_root_identity_permissions_and_symlink_are_fail_closed() {
    for mutation in ["identity", "mode", "symlink"] {
        let (_temp, root) = root();
        let path = trash(&root, "pending");
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "keep").unwrap();
        let s = Service::new();
        s.0.stop().unwrap();
        s.0.wake_root(&root).unwrap();
        let mut saved = None;
        match mutation {
            "identity" => fs::write(root.parent.join(IDENTITY_MARKER), "other").unwrap(),
            "mode" => fs::set_permissions(&root.parent, fs::Permissions::from_mode(0o755)).unwrap(),
            _ => {
                let p = root.parent.with_extension("saved");
                fs::rename(&root.parent, &p).unwrap();
                symlink(outside.path(), &root.parent).unwrap();
                saved = Some(p);
            }
        }
        s.0.resume().unwrap();
        s.pass();
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"keep");
        assert!(
            saved
                .as_ref()
                .map_or_else(|| path.clone(), |p| p.join(".trash-pending"))
                .join("file")
                .exists()
        );
        if saved.is_some() {
            fs::remove_file(&root.parent).unwrap();
        }
    }
}
#[test]
fn readonly_dirs_and_root_intermediate_leaf_symlinks_do_not_escape() {
    let (_temp, root) = root();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), "keep").unwrap();
    let path = trash(&root, "readonly");
    fs::create_dir(path.join("nested")).unwrap();
    symlink(outside.path(), path.join("nested/external")).unwrap();
    symlink(outside.path().join("keep"), path.join("leaf")).unwrap();
    fs::set_permissions(path.join("nested"), fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o555)).unwrap();
    symlink(outside.path(), root.parent.join(".trash-rootlink")).unwrap();
    let s = Service::new();
    s.0.wake_root(&root).unwrap();
    s.pass();
    assert!(!path.exists());
    assert!(!root.parent.join(".trash-rootlink").exists());
    assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"keep");
}
#[test]
fn exact_requests_survive_producer_disappearance_and_never_scan_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("created");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(target.join("file"), "remove").unwrap();
    fs::write(temp.path().join("keep"), "keep").unwrap();
    let s = Service::new();
    s.0.request_exact(&target, created_identity(&target).unwrap());
    s.pass();
    assert!(!target.exists());
    assert_eq!(fs::read(temp.path().join("keep")).unwrap(), b"keep");
}

#[test]
fn collector_spawn_failure_retries_on_next_notification_and_never_duplicates_worker() {
    let (_temp, root) = root();
    let path = trash(&root, "spawn");
    let s = Service::new();
    s.0.fail_start.store(true, Ordering::SeqCst);
    assert!(s.0.wake_root(&root).is_err());
    assert!(lock(&s.0.lifecycle).join.is_none());
    assert!(path.exists());
    s.0.wake_root(&root).unwrap();
    s.pass();
    assert!(!path.exists());
    let id = lock(&s.0.lifecycle).join.as_ref().unwrap().thread().id();
    for _ in 0..100 {
        s.0.wake_root(&root).unwrap();
    }
    assert_eq!(
        lock(&s.0.lifecycle).join.as_ref().unwrap().thread().id(),
        id
    );
}

#[test]
fn failed_tree_is_not_retried_in_the_same_wake() {
    let (_temp, root) = root();
    let path = trash(&root, "blocked");
    fs::create_dir(path.join("blocked")).unwrap();
    fs::write(path.join("blocked/keep"), "keep").unwrap();
    fs::set_permissions(path.join("blocked"), fs::Permissions::from_mode(0o0)).unwrap();
    let s = Service::new();
    s.0.wake_root(&root).unwrap();
    s.pass();
    assert!(path.exists());
    assert!(!lock(&s.0.shared.work).roots.get(&root.parent).unwrap().1);
    s.0.stop().unwrap();
    assert!(
        matches!(s.1.recv().unwrap(), TestEvent::Stopped),
        "failed tree self-woke"
    );
    fs::set_permissions(path.join("blocked"), fs::Permissions::from_mode(0o700)).unwrap();
    s.0.resume().unwrap();
    s.pass();
    assert!(!path.exists());
}

#[test]
fn root_and_intermediate_swap_during_walk_never_follow_replacement_links() {
    for swap_root in [false, true] {
        let (_temp, root) = root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "keep").unwrap();
        let path = root.parent.join(".trash-swap");
        fs::create_dir_all(path.join("nested")).unwrap();
        fs::write(path.join("nested/file"), "remove").unwrap();
        let target = if swap_root {
            path.clone()
        } else {
            path.join("nested")
        };
        let saved = root.parent.join("moved-original");
        let mut s = Service::new();
        let first = AtomicBool::new(true);
        s.0.step = Some(Arc::new({
            let outside = outside.path().to_path_buf();
            move || {
                if first.swap(false, Ordering::SeqCst) {
                    fs::rename(&target, &saved).unwrap();
                    symlink(&outside, &target).unwrap();
                }
            }
        }));
        s.0.wake_root(&root).unwrap();
        s.pass();
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"keep");
        assert!(path.symlink_metadata().is_ok());
    }
}

#[test]
fn non_regular_owner_marker_is_rejected_without_reading() {
    // An open socket has no EOF: reading it would hang, not validate an identity.
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let error = super::super::read_marker_fd(socket.into()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn missing_tree_completes_but_enumeration_and_io_errors_preserve_data() {
    let temp = tempfile::tempdir().unwrap();
    let parent = open(temp.path(), FLAGS, Mode::empty()).unwrap();
    let shared = Shared::default();
    let mut pace = Pace {
        shared: &shared,
        delay: Duration::ZERO,
        entries: 0,
        step: None,
        enumeration_error: None,
        throttle: None,
    };
    assert_eq!(
        remove_tree(&parent, c"missing", None, &mut pace).unwrap(),
        Outcome::Complete
    );
    let path = temp.path().join("tree");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), "keep").unwrap();
    for error in [Errno::IO, Errno::ACCESS] {
        pace.enumeration_error = Some(error);
        assert_eq!(
            remove_tree(&parent, c"tree", None, &mut pace)
                .unwrap_err()
                .raw_os_error(),
            Some(error.raw_os_error())
        );
        assert_eq!(fs::read(path.join("keep")).unwrap(), b"keep");
    }
}

#[test]
fn last_scratch_drop_deletes_without_a_living_producer() {
    let scratch = super::super::Scratch::new().unwrap();
    let path = scratch.path().to_path_buf();
    fs::write(path.join("file"), "remove").unwrap();
    let (tx, rx) = mpsc::channel();
    lock(&service().shared.work)
        .completed
        .entry(path.clone())
        .or_default()
        .push(tx);
    drop(scratch);
    rx.recv().unwrap();
    assert!(!path.exists());
    let root = ScratchRoot::ephemeral().unwrap();
    let parent = root.parent().to_path_buf();
    root.prepare("thread").unwrap();
    let (tx, rx) = mpsc::channel();
    lock(&service().shared.work)
        .completed
        .entry(parent.clone())
        .or_default()
        .push(tx);
    drop(root);
    rx.recv().unwrap();
    assert!(!parent.exists());
}

#[test]
fn stop_interrupts_throttle_without_waiting_for_its_delay() {
    let (_temp, root) = root();
    let path = trash(&root, "throttle");
    for i in 0..300 {
        fs::write(path.join(i.to_string()), "data").unwrap();
    }
    let mut s = Service::new();
    // Inject a long production pacing interval; completion is a stop event, never elapsed time.
    s.0.delay = Duration::from_secs(3600);
    s.0.wake_root(&root).unwrap();
    while !matches!(s.1.recv().unwrap(), TestEvent::Throttle) {}
    s.0.stop().unwrap();
    assert!(path.exists());
    while !matches!(s.1.recv().unwrap(), TestEvent::Stopped) {}
}

#[test]
fn completing_old_exact_request_does_not_erase_a_recreated_directory_request() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("created");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(path.join("file"), "old").unwrap();
    let original = created_identity(&path).unwrap();
    let mut s = Service::new();
    let shared = Arc::clone(&s.0.shared);
    let target = path.clone();
    let steps = AtomicUsize::new(0);
    s.0.step = Some(Arc::new(move || {
        if steps.fetch_add(1, Ordering::SeqCst) == 1 {
            fs::create_dir(&target).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(target.join("file"), "new").unwrap();
            let id = created_identity(&target).unwrap();
            assert_ne!(id, original, "old directory fd still pins its inode");
            let mut work = lock(&shared.work);
            work.exact.insert(target.clone(), id);
            work.exact_dirty = true;
            shared.changed.notify_all();
        }
    }));
    s.0.request_exact(&path, original);
    s.pass();
    s.pass();
    assert!(!path.exists());
}
