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
    assert!(status.stale(observed_at + STALE_AFTER));
    assert!(status.display(observed_at + STALE_AFTER).warning);
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

fn owned_with_expiry(writable: bool, expires_at: u64) -> OwnershipSnapshot {
    let OwnershipSnapshot::Owned {
        mut owner,
        writable,
    } = owned(writable)
    else {
        unreachable!()
    };
    owner.lease.expires_at = expires_at;
    OwnershipSnapshot::Owned { owner, writable }
}

/// Run egui passes until no repaint is pending.
fn settle(ctx: &egui::Context) {
    while ctx.has_requested_repaint() {
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
    }
}

#[test]
fn only_a_changed_or_urgent_probe_result_requests_a_repaint() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(
        OwnerHost::open(
            dir.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(64)),
        )
        .unwrap(),
    );
    let (script, outcomes) = mpsc::channel::<Result<OwnershipSnapshot, RegistryError>>();
    let worker = ProbeWorker::start(move |_, _, _| outcomes.recv().unwrap()).unwrap();
    let ctx = egui::Context::default();
    // The worker requests the repaint before it sends the result, so the
    // answer is already visible once the result has been received.
    let probe = |outcome: Result<OwnershipSnapshot, RegistryError>, urgent: bool| {
        settle(&ctx);
        script.send(outcome).unwrap();
        worker
            .requests
            .send(ProbeRequest {
                revision: 0,
                thread: "thread".into(),
                readonly: false,
                host: host.clone(),
                repaint: ctx.clone(),
                urgent,
            })
            .unwrap();
        worker.results.recv().unwrap();
        ctx.has_requested_repaint()
    };
    // The first result has nothing to be equal to.
    assert!(probe(Ok(owned(true)), false));
    // Same ownership twice, even with a renewed lease: nothing to redraw.
    assert!(!probe(Ok(owned(true)), false));
    assert!(!probe(Ok(owned_with_expiry(true, 1)), false));
    // An unknown, stale or superseded display needs even an unchanged result.
    assert!(probe(Ok(owned(true)), true));
    assert!(probe(Ok(owned(false)), false));
    assert!(probe(Ok(OwnershipSnapshot::Unowned), false));
    // A missing owner row is the same observation as Unowned.
    assert!(!probe(Err(RegistryError::Absent), false));
    // A repeated failure is shown the same way; recovery is a change.
    assert!(probe(Err(sql_error(rusqlite::ffi::SQLITE_BUSY)), false));
    assert!(!probe(Err(sql_error(rusqlite::ffi::SQLITE_BUSY)), false));
    assert!(probe(Ok(OwnershipSnapshot::Unowned), false));
}

#[test]
fn next_check_is_the_earliest_of_probe_stale_and_grace_deadlines() {
    let now = Instant::now();
    let ago = |millis| now - Duration::from_millis(millis);
    let after = Duration::from_millis;
    let mut status = OwnershipStatus::default();
    // Nothing probed yet: poll for the pending result, never spin.
    assert_eq!(status.next_check_in(now), RESULT_RECHECK);
    status.observe("thread", Ok(owned(true)), now);
    status.mark_probe_outstanding(ago(900));
    assert_eq!(status.next_check_in(now), after(100));
    // An overdue probe falls back to polling instead of repainting at once.
    status.mark_probe_outstanding(ago(1500));
    assert_eq!(status.next_check_in(now), RESULT_RECHECK);
    status.mark_probe_outstanding(now);
    assert_eq!(status.next_check_in(now), PROBE_INTERVAL);
    // The snapshot turns stale before the next probe.
    status.observe("thread", Ok(owned(true)), now - STALE_AFTER + after(200));
    assert_eq!(status.next_check_in(now), after(200));
    // A contention failure reaches its grace before that.
    status.observe(
        "thread",
        Err(sql_error(rusqlite::ffi::SQLITE_BUSY)),
        ago(850),
    );
    assert_eq!(status.next_check_in(now), after(150));
    status.observe("thread", Ok(owned(true)), now);
    status.observe(
        "thread",
        Err(sql_error(rusqlite::ffi::SQLITE_BUSY)),
        ago(900),
    );
    assert_eq!(status.next_check_in(now), after(100));
    // Already-warning states have no time-driven change left.
    status.observe("thread", Ok(owned(true)), now);
    status.observe("thread", Err(RegistryError::ReaderPoisoned), now);
    assert_eq!(status.next_check_in(now), PROBE_INTERVAL);
    status.invalidate();
    assert_eq!(status.next_check_in(now), RESULT_RECHECK);
}

#[test]
fn an_unchanged_result_is_not_stale_when_the_next_probe_pass_collects_it() {
    // Unchanged results are only collected at the next probe pass, so the
    // displayed age is a whole probe interval and must stay within the grace.
    let now = Instant::now();
    let mut status = OwnershipStatus::default();
    status.observe("thread", Ok(owned(true)), now);
    assert!(!status.stale(now + PROBE_INTERVAL + CONTENTION_GRACE / 2));
    assert!(status.stale(now + STALE_AFTER));
}

fn test_host(dir: &tempfile::TempDir) -> Arc<OwnerHost> {
    Arc::new(
        OwnerHost::open(
            dir.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(64)),
        )
        .unwrap(),
    )
}

/// A status wired to a real worker whose probe reports ownership of `thread`
/// as writable exactly when the request is not read-only.
fn status_with_worker() -> OwnershipStatus {
    let worker = ProbeWorker::start(|_, _, readonly| Ok(owned(!readonly))).unwrap();
    let mut status = OwnershipStatus {
        worker: Some(worker),
        ..Default::default()
    };
    status.select_thread(Some("thread"));
    status
}

/// Run UI passes at the logical time `now` until the outstanding probe has
/// been collected. The worker emits no event for an unchanged result (that is
/// the point), so the completion is only observable through `refresh` itself.
fn refresh_until_collected(
    status: &mut OwnershipStatus,
    host: &Arc<OwnerHost>,
    ctx: &egui::Context,
    readonly: bool,
    now: Instant,
) {
    while status.in_flight {
        status.refresh(host.clone(), "thread", readonly, ctx, now);
        std::thread::yield_now();
    }
}

#[test]
fn an_unchanged_result_is_collected_by_the_next_pass_without_waking_the_ui() {
    let dir = tempfile::tempdir().unwrap();
    let host = test_host(&dir);
    let ctx = egui::Context::default();
    let mut status = status_with_worker();
    // The first result has an unknown display, so it wakes the UI.
    let t0 = Instant::now();
    status.refresh(host.clone(), "thread", false, &ctx, t0);
    assert!(status.in_flight);
    refresh_until_collected(&mut status, &host, &ctx, false, t0);
    assert_eq!(status.snapshot, Some(owned(true)));
    assert!(ctx.has_requested_repaint());
    settle(&ctx);
    let first_observation = status.last_observed.unwrap();
    // The next probe pass sends a probe whose result will not change anything.
    let t1 = t0 + PROBE_INTERVAL;
    status.refresh(host.clone(), "thread", false, &ctx, t1);
    assert!(status.in_flight);
    assert_eq!(status.last_probe, Some(t1));
    refresh_until_collected(&mut status, &host, &ctx, false, t1);
    // The worker finished before refresh collected it, yet nothing was requested.
    assert!(!ctx.has_requested_repaint());
    // The collecting pass recorded the new observation and freed the probe slot.
    let observation = status.last_observed.unwrap();
    assert!(observation > first_observation);
    assert!(!status.in_flight);
    // Displayed one probe interval after its observation it is not yet stale.
    assert!(!status.stale(observation + PROBE_INTERVAL));
    assert!(status.stale(observation + STALE_AFTER));
    // Probing continues at the usual cadence.
    let t2 = t1 + PROBE_INTERVAL;
    status.refresh(host, "thread", false, &ctx, t2);
    assert!(status.in_flight);
    assert_eq!(status.last_probe, Some(t2));
}

#[test]
fn a_result_for_a_superseded_readonly_state_is_dropped_and_the_latest_one_shown() {
    let dir = tempfile::tempdir().unwrap();
    let host = test_host(&dir);
    let ctx = egui::Context::default();
    let mut status = status_with_worker();
    let t0 = Instant::now();
    status.refresh(host.clone(), "thread", false, &ctx, t0);
    // The UI now wants a read-only view. The writable answer is dropped when
    // collected, and the same pass asks again with the latest conditions.
    let t1 = t0 + Duration::from_millis(1);
    while status.last_probe != Some(t1) {
        status.refresh(host.clone(), "thread", true, &ctx, t1);
        std::thread::yield_now();
    }
    assert!(status.snapshot.is_none());
    assert!(status.in_flight);
    refresh_until_collected(&mut status, &host, &ctx, true, t1);
    assert_eq!(status.snapshot, Some(owned(false)));
    assert_eq!(status.display(t1).access, "read-only");
}
