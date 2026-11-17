use super::*;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
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
    s.0.request_exact(&target);
    s.pass();
    assert!(!target.exists());
    assert_eq!(fs::read(temp.path().join("keep")).unwrap(), b"keep");
}
