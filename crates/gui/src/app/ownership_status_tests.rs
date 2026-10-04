use super::*;
use runtime::ownership::Lease;

fn owned(writable: bool) -> OwnershipSnapshot {
    OwnershipSnapshot::Owned {
        owner: ThreadOwner::new(
            "thread".into(),
            Lease {
                owner_id: "owner-long-id".into(),
                generation: 3,
                expires_at: u64::MAX,
            },
        ),
        writable,
    }
}

fn sql_error(code: i32) -> RegistryError {
    rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some("probe detail".into()))
        .into()
}

#[test]
fn busy_and_locked_preserve_confirmed_access_until_grace_expires() {
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
    ] {
        for writable in [true, false] {
            let now = Instant::now();
            let mut status = OwnershipStatus::default();
            status.observe("thread", Ok(owned(writable)), now);
            status.observe("thread", Err(sql_error(code)), now);
            let access = if writable { "write" } else { "read-only" };
            assert_eq!(
                status.display(now),
                OwnershipDisplay {
                    access,
                    warning: false
                }
            );
            assert!(status.failure.as_ref().unwrap().contention);
            let later = now + Duration::from_millis(999);
            status.observe("thread", Err(sql_error(code)), later);
            assert_eq!(status.failure.as_ref().unwrap().since, now);
            assert!(!status.display(later).warning);
            assert_eq!(
                status.display(now + CONTENTION_GRACE),
                OwnershipDisplay {
                    access,
                    warning: true
                }
            );
            assert_eq!(status.snapshot, Some(owned(writable)));
        }
    }
}

#[test]
fn non_contention_errors_warn_immediately_without_string_classification() {
    for error in [
        sql_error(rusqlite::ffi::SQLITE_CORRUPT),
        RegistryError::ReaderPoisoned,
        std::io::Error::other("database is locked").into(),
    ] {
        let now = Instant::now();
        let mut status = OwnershipStatus::default();
        status.observe("thread", Ok(owned(true)), now);
        let detail = error.to_string();
        status.observe("thread", Err(error), now);
        assert_eq!(
            status.display(now),
            OwnershipDisplay {
                access: "write",
                warning: true
            }
        );
        let failure = status.failure.unwrap();
        assert!(!failure.contention);
        assert_eq!(failure.detail, detail);
    }
}

#[test]
fn success_clears_failure_and_starts_a_fresh_grace_period() {
    for elapsed in [Duration::from_millis(200), Duration::from_secs(2)] {
        let now = Instant::now();
        let mut status = OwnershipStatus::default();
        status.observe("thread", Ok(owned(true)), now);
        status.observe("thread", Err(sql_error(rusqlite::ffi::SQLITE_BUSY)), now);
        let recovered = now + elapsed;
        status.observe("thread", Ok(owned(false)), recovered);
        assert!(status.failure.is_none());
        assert_eq!(
            status.display(recovered),
            OwnershipDisplay {
                access: "read-only",
                warning: false
            }
        );
        status.observe(
            "thread",
            Err(sql_error(rusqlite::ffi::SQLITE_LOCKED)),
            recovered,
        );
        assert_eq!(status.failure.as_ref().unwrap().since, recovered);
        assert!(!status.display(recovered).warning);
        status.observe("thread", Err(RegistryError::Absent), recovered);
        assert!(status.failure.is_none());
        assert_eq!(status.snapshot, Some(OwnershipSnapshot::Unowned));
        assert_eq!(status.display(recovered).access, "read-only");
    }
}

#[test]
fn startup_failure_checks_ownership_instead_of_inventing_read_only() {
    let now = Instant::now();
    let mut status = OwnershipStatus::default();
    assert_eq!(
        status.display(now),
        OwnershipDisplay {
            access: "checking ownership",
            warning: false
        }
    );
    status.observe("thread", Err(sql_error(rusqlite::ffi::SQLITE_BUSY)), now);
    assert!(status.snapshot.is_none());
    assert_eq!(
        status.display(now),
        OwnershipDisplay {
            access: "checking ownership",
            warning: false
        }
    );
    assert_eq!(
        status.display(now + CONTENTION_GRACE),
        OwnershipDisplay {
            access: "checking ownership",
            warning: true
        }
    );
}

#[test]
fn switching_threads_or_deselecting_resets_snapshot_and_failure() {
    let now = Instant::now();
    let mut status = OwnershipStatus::default();
    status.observe("first", Ok(owned(true)), now);
    status.observe("first", Err(RegistryError::ReaderPoisoned), now);
    let later = now + Duration::from_secs(5);
    status.observe("second", Err(sql_error(rusqlite::ffi::SQLITE_BUSY)), later);
    assert_eq!(status.thread.as_deref(), Some("second"));
    assert!(status.snapshot.is_none());
    assert_eq!(status.failure.as_ref().unwrap().since, later);
    assert_eq!(
        status.display(later),
        OwnershipDisplay {
            access: "checking ownership",
            warning: false
        }
    );
    status.select_thread(Some("first"));
    assert!(status.snapshot.is_none());
    assert!(status.failure.is_none());
    status.observe("first", Ok(owned(false)), later);
    status.select_thread(None);
    assert!(status.thread.is_none());
    assert!(status.snapshot.is_none());
    assert!(status.failure.is_none());
}

#[test]
fn blocked_probe_keeps_refresh_nonblocking_and_coalesces_thread_changes() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(
        OwnerHost::open(
            dir.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(64)),
        )
        .unwrap(),
    );
    let (started, starts) = mpsc::channel();
    let (release, releases) = mpsc::channel();
    let worker = ProbeWorker::start(move |_, thread, readonly| {
        started.send((thread.to_owned(), readonly)).unwrap();
        releases.recv().unwrap();
        Ok(owned(!readonly))
    })
    .unwrap();
    let mut status = OwnershipStatus {
        worker: Some(worker),
        ..Default::default()
    };
    let ctx = egui::Context::default();
    let now = Instant::now();
    status.select_thread(Some("first"));
    status.refresh(host.clone(), "first", false, &ctx, now);
    assert_eq!(starts.recv().unwrap(), ("first".into(), false));
    // The probe cannot finish until release; these calls must never wait for it.
    for thread in ["second", "third", "first"] {
        status.select_thread(Some(thread));
        status.refresh(host.clone(), thread, false, &ctx, now);
        assert_eq!(status.display(now).access, "checking ownership");
    }
    assert!(matches!(starts.try_recv(), Err(mpsc::TryRecvError::Empty)));
    release.send(()).unwrap();
    let result = status.worker.as_ref().unwrap().results.recv().unwrap();
    status.accept_result(result, "first", false);
    // Returning to the original thread must not accept its pre-switch result.
    assert!(status.snapshot.is_none());
    status.refresh(host, "first", true, &ctx, now);
    assert_eq!(starts.recv().unwrap(), ("first".into(), true));
    release.send(()).unwrap();
    let result = status.worker.as_ref().unwrap().results.recv().unwrap();
    let observed_at = result.observed_at;
    status.accept_result(result, "first", true);
    assert_eq!(status.display(observed_at).access, "read-only");
    assert!(!status.stale(observed_at));
    assert!(status.stale(observed_at + CONTENTION_GRACE));
    assert!(status.display(observed_at + CONTENTION_GRACE).warning);
}

#[test]
fn readonly_change_and_action_invalidation_discard_pending_write_results() {
    let now = Instant::now();
    let mut status = OwnershipStatus::default();
    status.select_thread(Some("thread"));
    let result = || ProbeResult {
        revision: status.revision,
        thread: "thread".into(),
        readonly: false,
        result: Ok(owned(true)),
        observed_at: now,
    };
    let readonly_changed = result();
    let action_changed = result();
    status.accept_result(readonly_changed, "thread", true);
    assert!(status.snapshot.is_none());
    status.observe("thread", Ok(owned(true)), now);
    assert!(!status.stale(now));
    status.invalidate();
    assert!(status.stale(now));
    assert!(status.display(now).warning);
    status.accept_result(action_changed, "thread", false);
    assert_eq!(status.snapshot, Some(owned(true)));
    assert!(status.stale(now));
    status.observe("thread", Ok(owned(false)), now);
    assert!(!status.stale(now));
    assert!(!status.display(now).warning);
}
