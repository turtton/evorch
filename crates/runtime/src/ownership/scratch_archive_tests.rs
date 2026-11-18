use super::*;
use crate::{
    RunId,
    ownership::OwnerHost,
    scratch::{ScratchKey, usage::ScratchUser},
};
use std::{
    fs,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
struct Fixture {
    host: OwnerHost,
    scratch: Arc<ThreadScratch>,
    key: ScratchKey,
    _dir: test_socket_dir::SocketDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = test_socket_dir::SocketDir::new();
        let host = OwnerHost::open(
            dir.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(32)),
        )
        .unwrap();
        host.start("thread").unwrap();
        let scratch = host.thread_scratch();
        let key = ScratchKey::Thread("thread".into());
        let path = scratch.prepare(&key).unwrap();
        fs::write(path.join("keep"), "scratch").unwrap();
        Self {
            host,
            scratch,
            key,
            _dir: dir,
        }
    }
    fn db(&self) -> PathBuf {
        self._dir.path().join("owners.db")
    }
    fn present(&self) {
        assert_eq!(
            fs::read(self.scratch.dir(&self.key).join("keep")).unwrap(),
            b"scratch"
        );
    }
    fn attempt(&self, checker: Checker) -> Result<bool, RegistryError> {
        attempt(
            &self.db(),
            self.host.owner_id(),
            &self.scratch,
            "thread",
            &checker,
        )
    }
}
fn archived() -> Checker {
    Arc::new(|_| Ok(true))
}
fn run(n: u64) -> RunId {
    RunId::new(n)
}
#[test]
fn independent_run_child_and_shell_users_block_before_sidebar_read() {
    let f = Fixture::new();
    let parent = run(1);
    let child = run(2);
    f.scratch
        .register_execution(&f.key, parent, "parent")
        .unwrap();
    f.scratch
        .register_execution(&f.key, child, "child")
        .unwrap();
    let shell = ScratchUser::Shell {
        run: parent,
        id: "job".into(),
    };
    f.scratch.register_user(&f.key, shell.clone()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let checker: Checker = Arc::new({
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
    });
    assert!(!f.attempt(checker.clone()).unwrap());
    f.scratch.release_execution(parent, "parent");
    assert!(!f.attempt(checker.clone()).unwrap());
    f.scratch.release_execution(child, "child");
    assert!(!f.attempt(checker.clone()).unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    f.present();
    f.scratch.release_user(&f.key, &shell);
    assert!(f.attempt(checker).unwrap());
    assert!(!f.scratch.dir(&f.key).exists());
}
#[test]
fn owner_generation_planned_and_orphan_holds_each_preserve_contents() {
    for condition in ["foreign", "generation", "planned", "orphan"] {
        let f = Fixture::new();
        Registry::open_existing(&f.db())
            .unwrap()
            .update("thread", |owner| {
                match condition {
                    "foreign" => owner.lease.owner_id = "foreign".into(),
                    "generation" => owner.lease.generation += 1,
                    _ => {
                        owner.install_hold("tx", "capture", &Default::default())?;
                        if condition == "orphan" {
                            owner.handoff_hold.as_mut().unwrap().orphaned = true;
                        }
                    }
                }
                Ok(())
            })
            .unwrap();
        assert!(!f.attempt(archived()).unwrap(), "{condition}");
        f.present();
    }
}
#[test]
fn epoch_change_even_when_new_user_has_already_left_prevents_detachment() {
    let f = Fixture::new();
    let scratch = f.scratch.clone();
    let key = f.key.clone();
    let checker: Checker = Arc::new(move |_| {
        scratch.register_execution(&key, run(3), "new-incarnation")?;
        scratch.release_execution(run(3), "new-incarnation");
        Ok(true)
    });
    assert!(!f.attempt(checker).unwrap());
    f.present();
    assert!(f.attempt(archived()).unwrap());
}
#[test]
fn hold_installed_during_sidebar_read_wins_write_reservation() {
    let f = Fixture::new();
    let db = f.db();
    let checker: Checker = Arc::new(move |_| {
        Registry::open_existing(&db)
            .unwrap()
            .update("thread", |owner| {
                owner.install_hold("tx", "capture", &Default::default())
            })
            .unwrap();
        Ok(true)
    });
    assert!(!f.attempt(checker).unwrap());
    f.present();
}
#[test]
fn missing_sidebar_and_checker_failure_are_not_permission_to_delete() {
    let f = Fixture::new();
    f.host.hint_scratch_archive("thread");
    f.host.scratch_archive_barrier().recv().unwrap();
    f.present();
    assert!(
        f.attempt(Arc::new(|_| Err(io::Error::other("unreadable sidebar"))))
            .is_err()
    );
    f.present();
}
#[test]
fn archive_then_unarchive_while_busy_removes_the_hint_on_last_release() {
    let f = Fixture::new();
    let active = run(4);
    f.scratch.register_execution(&f.key, active, "one").unwrap();
    let saved = Arc::new(AtomicBool::new(true));
    f.host.set_scratch_archive_checker(Arc::new({
        let saved = saved.clone();
        move |_| Ok(saved.load(Ordering::SeqCst))
    }));
    f.host.hint_scratch_archive("thread");
    f.host.scratch_archive_barrier().recv().unwrap();
    f.present();
    saved.store(false, Ordering::SeqCst);
    f.scratch.release_execution(active, "one");
    f.host.scratch_archive_barrier().recv().unwrap();
    f.present();
    // No explicit unarchive invalidation: the actual attempt consumed the hint.
    saved.store(true, Ordering::SeqCst);
    f.scratch.register_execution(&f.key, active, "one").unwrap();
    f.scratch.release_execution(active, "one");
    f.host.scratch_archive_barrier().recv().unwrap();
    f.present();
}
#[test]
fn archive_worker_not_caller_runs_checker_and_detaches() {
    let f = Fixture::new();
    f.host.set_scratch_archive_checker(Arc::new(|_| {
        assert_eq!(
            std::thread::current().name(),
            Some("evorch-scratch-archive")
        );
        Ok(true)
    }));
    f.host.hint_scratch_archive("thread");
    f.host.scratch_archive_barrier().recv().unwrap();
    assert!(!f.scratch.dir(&f.key).exists());
}
#[test]
fn archive_failure_does_not_advance_generation_or_report_disappearance() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let before = Registry::open_existing(&f.db())
        .unwrap()
        .attach("thread")
        .unwrap()
        .lease
        .generation;
    fs::set_permissions(f.scratch.parent(), fs::Permissions::from_mode(0o500)).unwrap();
    assert!(f.attempt(archived()).is_err());
    fs::set_permissions(f.scratch.parent(), fs::Permissions::from_mode(0o700)).unwrap();
    f.present();
    assert_eq!(
        Registry::open_existing(&f.db())
            .unwrap()
            .attach("thread")
            .unwrap()
            .lease
            .generation,
        before
    );
    assert!(!f.scratch.take_archived_notice(&f.key, run(5)));
}
#[test]
fn old_incarnation_cannot_release_a_new_incarnations_user() {
    let f = Fixture::new();
    let id = run(6);
    f.scratch.register_execution(&f.key, id, "old").unwrap();
    f.scratch.register_execution(&f.key, id, "new").unwrap();
    f.scratch.release_execution(id, "old");
    assert!(!f.attempt(archived()).unwrap());
    f.present();
    f.scratch.release_execution(id, "new");
    assert!(f.attempt(archived()).unwrap());
}
#[test]
fn archived_notice_once_per_run_and_same_path_reprepared_empty() {
    let f = Fixture::new();
    let path = f.scratch.dir(&f.key);
    let id = run(7);
    assert!(f.attempt(archived()).unwrap());
    f.scratch.register_execution(&f.key, id, "new").unwrap();
    assert_eq!(f.scratch.prepare(&f.key).unwrap(), path);
    assert!(!path.join("keep").exists());
    assert!(f.scratch.take_archived_notice(&f.key, id));
    assert!(!f.scratch.take_archived_notice(&f.key, id));
}

#[test]
fn registry_busy_retains_data_then_uses_one_injected_backoff_before_external_retry() {
    for release_during_backoff in [true, false] {
        let f = Fixture::new();
        let connection = rusqlite::Connection::open(f.db()).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        let state = Arc::new(Mutex::new(State {
            pending: HashSet::from(["thread".into()]),
            checker: Some(archived()),
            ..Default::default()
        }));
        let (wake, rx) = mpsc::sync_channel(1);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let waits = Arc::new(AtomicUsize::new(0));
        let worker = std::thread::spawn({
            let state = Arc::clone(&state);
            let scratch = Arc::clone(&f.scratch);
            let db = f.db();
            let owner = f.host.owner_id().to_owned();
            let waits = Arc::clone(&waits);
            move || {
                serve(db, owner, scratch, state, rx, move |_| {
                    waits.fetch_add(1, Ordering::SeqCst);
                    entered_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Err(mpsc::RecvTimeoutError::Timeout)
                })
            }
        });
        wake.send(()).unwrap();
        entered_rx.recv().unwrap(); // First write reservation was Busy, no filesystem detachment.
        f.present();
        let (pass_tx, pass_rx) = mpsc::channel();
        lock(&state).barriers.push(pass_tx);
        if release_during_backoff {
            connection.execute_batch("COMMIT").unwrap();
        }
        resume_tx.send(()).unwrap();
        pass_rx.recv().unwrap();
        assert_eq!(waits.load(Ordering::SeqCst), 1);
        if !release_during_backoff {
            f.present();
            assert!(lock(&state).pending.contains("thread"));
            connection.execute_batch("COMMIT").unwrap();
            let (pass_tx, pass_rx) = mpsc::channel();
            lock(&state).barriers.push(pass_tx);
            wake.send(()).unwrap(); // Only this external event permits another attempt.
            pass_rx.recv().unwrap();
        }
        assert!(!f.scratch.dir(&f.key).exists());
        lock(&state).stopped = true;
        wake.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(waits.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn a_new_archive_hint_survives_an_older_unarchived_result() {
    let f = Fixture::new();
    let entered = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    let calls = Arc::new(AtomicUsize::new(0));
    f.host.set_scratch_archive_checker(Arc::new({
        let entered = Arc::clone(&entered);
        let resume = Arc::clone(&resume);
        let calls = Arc::clone(&calls);
        move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                entered.wait();
                resume.wait();
                Ok(false)
            } else {
                Ok(true)
            }
        }
    }));
    f.host.hint_scratch_archive("thread");
    entered.wait();
    f.present();
    f.host.hint_scratch_archive("thread");
    let next_pass = f.host.scratch_archive_barrier();
    resume.wait();
    next_pass.recv().unwrap();
    assert!(!f.scratch.dir(&f.key).exists());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
