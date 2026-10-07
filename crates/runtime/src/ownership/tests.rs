use super::*;

#[cfg(unix)]
#[test]
fn host_release_allows_explicit_claim_and_fences_old_permit() {
    let directory = tempfile::tempdir().expect("directory");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let first = OwnerHost::open(
        directory.path(),
        config::OwnershipConfig::default(),
        bus.clone(),
    )
    .expect("first");
    let old = first.start("thread").expect("start");
    let second =
        OwnerHost::open(directory.path(), config::OwnershipConfig::default(), bus).expect("second");
    assert!(second.owned_permit("thread").is_err());
    assert!(
        second
            .claim(&second.attach("thread").expect("attach"))
            .is_err()
    );
    assert!(!first.quiesce().expect("release"));
    let permit = second
        .claim(&second.attach("thread").expect("released"))
        .expect("claim");
    assert_eq!(permit.lease.generation, old.lease.generation + 1);
    assert!(old.begin_turn().is_err());
    permit.begin_turn().expect("new owner");
    permit.checkpoint(&[]).expect("checkpoint");
}

#[test]
fn quiesce_waits_for_every_child_run_checkpoint() {
    let mut owner = owner();
    let lease = owner.lease.clone();
    owner.begin_run(&lease, "parent", 1).expect("parent");
    owner.begin_run(&lease, "child", 1).expect("child");
    owner.quiesce(&lease).expect("quiesce");
    owner
        .checkpoint_run(&lease, "parent")
        .expect("parent checkpoint");
    assert!(owner.release(&lease).is_err());
    owner
        .checkpoint_run(&lease, "child")
        .expect("child checkpoint");
    owner.release(&lease).expect("release");
    assert_eq!(owner.state, OwnerState::Released);
}

fn owner() -> ThreadOwner {
    ThreadOwner::new(
        "thread-1".into(),
        Lease {
            owner_id: "a".into(),
            generation: 1,
            expires_at: 100,
        },
    )
}

#[test]
fn quiesce_blocks_new_turns_until_checkpoint_releases_owner() {
    // Given: an owner executing a turn.
    let mut owner = owner();
    let token = owner.lease.clone();
    owner.begin_turn(&token, 10).expect("begin");
    // When: shutdown is requested.
    owner.quiesce(&token).expect("quiesce");
    // Then: work is drained, not abruptly released.
    assert_eq!(owner.state, OwnerState::Quiescing);
    assert!(owner.begin_turn(&token, 11).is_err());
    assert!(owner.release(&token).is_err());
    owner.checkpoint(&token).expect("checkpoint");
    owner.release(&token).expect("release");
    assert_eq!(owner.state, OwnerState::Released);
}

#[test]
fn claim_reclaims_abandoned_active_turn_after_grace() {
    // Given: an expired owner with unfinished work.
    let mut owner = owner();
    owner.begin_turn(&owner.lease.clone(), 10).expect("begin");
    let old = owner.lease.clone();
    // When: the lease is stale (the IPC layer checks owner liveness).
    owner.claim(&old, "b", 200, 50).expect("reclaim");
    // Then: abandoned work is fenced and a fresh turn can begin.
    assert!(!owner.active_turn);
    assert!(owner.validate(&old).is_err());
    owner
        .begin_turn(&owner.lease.clone(), 200)
        .expect("new turn");
}

#[test]
fn claim_fences_previous_generation() {
    // Given: a stale, idle owner (liveness checked by the registry).
    let mut owner = owner();
    let old = owner.lease.clone();
    // When: ownership is claimed.
    owner.claim(&old, "b", 200, 50).expect("claim");
    // Then: old mutations and competing claims are rejected.
    assert_eq!(owner.lease.generation, 2);
    assert!(owner.begin_turn(&old, 200).is_err());
    assert!(owner.claim(&old, "c", 300, 50).is_err());
}

#[test]
fn permit_checkpoints_messages_before_clearing_active_turn() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut registry = Registry::open(&path).expect("registry");
    let mut original = owner();
    original.lease.expires_at = u64::MAX;
    registry.start(&original).expect("start");
    let permit = OwnerPermit {
        run_id: None,
        registry_path: path.clone(),
        thread_id: original.thread_id,
        lease: original.lease,
    };
    permit.begin_turn().expect("begin");
    permit.checkpoint(&[]).expect("checkpoint");
    assert!(!registry.attach("thread-1").expect("owner").active_turn);
    let connection = rusqlite::Connection::open(path).expect("connection");
    let payload: String = connection
        .query_row(
            "SELECT payload FROM owner_checkpoints WHERE thread_id='thread-1'",
            [],
            |row| row.get(0),
        )
        .expect("checkpoint row");
    assert_eq!(payload, "[]");
}

#[test]
fn registry_attach_never_starts_an_absent_thread() {
    let directory = tempfile::tempdir().expect("directory");
    let registry = Registry::open(&directory.path().join("owners.db")).expect("registry");
    assert!(matches!(
        registry.attach("absent"),
        Err(RegistryError::Absent)
    ));
}

#[test]
fn registry_commits_claim_and_rolls_back_competing_generation() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut registry = Registry::open(&path).expect("registry");
    let original = owner();
    registry.start(&original).expect("start");
    registry
        .update("thread-1", |owner| {
            owner.claim(&original.lease, "b", 200, 50)
        })
        .expect("claim");
    let mut other = Registry::open(&path).expect("other connection");
    assert!(
        other
            .update("thread-1", |owner| owner.claim(
                &original.lease,
                "c",
                200,
                50
            ))
            .is_err()
    );
    assert_eq!(
        other.attach("thread-1").expect("attach").lease.owner_id,
        "b"
    );
}

#[cfg(unix)]
#[test]
fn claim_refuses_reachable_owner_even_when_lease_is_stale() {
    let directory = tempfile::tempdir().expect("directory");
    let socket = directory.path().join("owner.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("listener");
    let mut registry = Registry::open(&directory.path().join("owners.db")).expect("registry");
    let original = owner();
    registry.start(&original).expect("start");
    let result = ipc::claim(
        &mut registry,
        ipc::ClaimRequest {
            thread_id: "thread-1",
            expected: &original.lease,
            previous_socket: &socket,
            owner_id: "b",
            now_ms: 200,
            grace_ms: 50,
        },
    );
    assert!(matches!(
        result,
        Err(RegistryError::Ownership(OwnershipError::OwnerResponsive))
    ));
    assert_eq!(registry.attach("thread-1").expect("attach"), original);
}

#[test]
fn missing_registry_probes_and_permits_never_create_a_database() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let owner = owner();
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: owner.thread_id,
        lease: owner.lease,
        run_id: None,
    };

    assert!(Registry::open_readonly(&path).is_err());
    assert!(Registry::open_existing(&path).is_err());
    assert!(permit.validate_generation().is_err());
    assert!(permit.validate_mutation().is_err());
    assert!(permit.mutation_guard().is_err());
    assert!(permit.begin_turn().is_err());
    assert!(permit.checkpoint(&[]).is_err());
    assert!(!path.exists());
}

#[test]
fn existing_registry_opens_do_not_initialize_schema() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let connection = rusqlite::Connection::open(&path).expect("empty database");

    assert!(
        Registry::open_existing(&path)
            .expect("existing database")
            .attach("thread")
            .is_err()
    );
    assert!(
        Registry::open_readonly(&path)
            .expect("readonly database")
            .attach("thread")
            .is_err()
    );
    let tables: i64 = connection
        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
        .expect("schema");
    assert_eq!(tables, 0);
}

#[test]
fn reused_readonly_registry_observes_external_generation_changes() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut writer = Registry::open(&path).expect("registry");
    let original = owner();
    writer.start(&original).expect("start");
    let mut reader = Registry::open_readonly(&path).expect("reader");
    assert_eq!(reader.attach("thread-1").expect("prime cache"), original);
    assert!(reader.start(&original).is_err());

    let next = writer
        .update("thread-1", |owner| {
            owner.claim(&original.lease, "b", 200, 50)
        })
        .expect("claim");
    assert_eq!(reader.attach("thread-1").expect("new generation"), next);
    assert_eq!(reader.list().expect("list"), vec![next]);
}

#[test]
fn readonly_mutation_guard_holds_the_generation_snapshot_until_dropped() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut registry = Registry::open(&path).expect("registry");
    let original = owner();
    registry.start(&original).expect("start");
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: original.thread_id.clone(),
        lease: original.lease.clone(),
        run_id: None,
    };
    let guard = permit.mutation_guard().expect("generation guard");
    let writer = rusqlite::Connection::open(&path).expect("independent writer");
    writer
        .busy_timeout(std::time::Duration::ZERO)
        .expect("immediate contention result");
    let error = writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE thread_owners SET state = state; COMMIT")
        .expect_err("guard must prevent committing a generation change");
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    writer.execute_batch("ROLLBACK").expect("rollback");
    drop(guard);

    registry
        .update("thread-1", |owner| {
            owner.claim(&original.lease, "b", 200, 50)
        })
        .expect("claim after guard is dropped");
    assert!(matches!(
        permit.validate_generation(),
        Err(RegistryError::Ownership(OwnershipError::Fenced))
    ));
}

#[test]
fn nonblocking_mutation_guard_distinguishes_contention_from_fencing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owners.db");
    let original = owner();
    let mut registry = Registry::open(&path).unwrap();
    registry.start(&original).unwrap();
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: original.thread_id.clone(),
        lease: original.lease.clone(),
        run_id: None,
    };
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(permit.try_mutation_guard().unwrap().is_none());
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(permit.try_mutation_guard().unwrap().is_some());
    registry
        .update("thread-1", |owner| {
            owner.claim(&original.lease, "new", 200, 50)
        })
        .unwrap();
    assert!(matches!(
        permit.try_mutation_guard(),
        Err(RegistryError::Ownership(OwnershipError::Fenced))
    ));
}

#[tokio::test]
async fn compound_event_fences_release_readers_before_waiting_and_revalidate_every_run() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use event_bus::{Event, EventBus, LifecycleEvent, MutationGuard, MutationGuardAttempt};

    struct ReadGuard {
        registry: Option<Registry>,
        held: Arc<AtomicUsize>,
        commit: Option<Arc<Mutex<rusqlite::Connection>>>,
        commits: Arc<AtomicUsize>,
    }
    impl Drop for ReadGuard {
        fn drop(&mut self) {
            drop(self.registry.take());
            self.held.fetch_sub(1, Ordering::SeqCst);
            if let Some(writer) = self.commit.take() {
                writer
                    .lock()
                    .unwrap()
                    .execute_batch("COMMIT")
                    .expect("releasing the partial read set lets the pending writer commit");
                self.commits.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    for entry in ["emit", "recv", "drain", "validator"] {
        for revoke in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("owners.db");
            let mut registry = Registry::open(&path).unwrap();
            let mut source = owner();
            source.thread_id = "source".into();
            registry.start(&source).unwrap();
            let mut child = owner();
            child.thread_id = "child".into();
            registry.start(&child).unwrap();
            let mut replacement = source.clone();
            replacement.lease.generation += 1;
            let replacement = serde_json::to_string(&replacement).unwrap();
            let writer = rusqlite::Connection::open(&path).unwrap();
            writer.busy_timeout(std::time::Duration::ZERO).unwrap();
            let writer = Arc::new(Mutex::new(writer));
            let armed = Arc::new(AtomicBool::new(false));
            let held = Arc::new(AtomicUsize::new(0));
            let commits = Arc::new(AtomicUsize::new(0));
            let busy = Arc::new(AtomicUsize::new(0));
            let waits = Arc::new(AtomicUsize::new(0));
            let bus = EventBus::new(8);
            let mut events = bus.subscribe();
            for owner in [source, child] {
                let run = owner.thread_id.clone();
                let permit = OwnerPermit {
                    registry_path: path.clone(),
                    thread_id: run.clone(),
                    lease: owner.lease,
                    run_id: None,
                };
                let wrap = {
                    let armed = Arc::clone(&armed);
                    let held = Arc::clone(&held);
                    let commits = Arc::clone(&commits);
                    let writer = Arc::clone(&writer);
                    let replacement = replacement.clone();
                    let source = run == "source";
                    Arc::new(move |registry: Registry| -> Box<dyn MutationGuard> {
                        held.fetch_add(1, Ordering::SeqCst);
                        let commit = if source && armed.swap(false, Ordering::SeqCst) {
                            let writer_guard = writer.lock().unwrap();
                            writer_guard.execute_batch("BEGIN IMMEDIATE").unwrap();
                            if revoke {
                                writer_guard
                                    .execute(
                                        "UPDATE thread_owners SET state = ?1 WHERE thread_id = 'source'",
                                        [&replacement],
                                    )
                                    .unwrap();
                            } else {
                                writer_guard
                                    .execute_batch("UPDATE thread_owners SET state = state")
                                    .unwrap();
                            }
                            let error = writer_guard
                                .execute_batch("COMMIT")
                                .expect_err("source reader forces the writer into PENDING");
                            assert_eq!(
                                error.sqlite_error_code(),
                                Some(rusqlite::ErrorCode::DatabaseBusy)
                            );
                            Some(Arc::clone(&writer))
                        } else {
                            None
                        };
                        Box::new(ReadGuard {
                            registry: Some(registry),
                            held: Arc::clone(&held),
                            commit,
                            commits: Arc::clone(&commits),
                        })
                    })
                };
                let blocking = {
                    let permit = permit.clone();
                    let wrap = Arc::clone(&wrap);
                    let held = Arc::clone(&held);
                    let waits = Arc::clone(&waits);
                    Arc::new(move || {
                        assert_eq!(
                            held.load(Ordering::SeqCst),
                            0,
                            "blocking ownership acquisition must never retain another reader"
                        );
                        waits.fetch_add(1, Ordering::SeqCst);
                        permit.mutation_guard().ok().map(|guard| wrap(guard))
                    })
                };
                let busy = Arc::clone(&busy);
                assert!(bus.register_nonblocking_mutation_guard(
                    run,
                    blocking,
                    Arc::new(move || match permit.try_mutation_guard() {
                        Ok(Some(guard)) => MutationGuardAttempt::Acquired(wrap(guard)),
                        Ok(None) => {
                            busy.fetch_add(1, Ordering::SeqCst);
                            MutationGuardAttempt::Busy
                        }
                        Err(_) => MutationGuardAttempt::Rejected,
                    }),
                ));
            }
            let event = Event::new(LifecycleEvent::EscalationRequested {
                source_run_id: "source".into(),
                new_run_id: "child".into(),
                summary: event_bus::EscalationMemoSummary {
                    original_request: String::new(),
                    escalation_reason: String::new(),
                    files_touched: Vec::new(),
                    blockers: Vec::new(),
                    suggested_next: String::new(),
                },
            });
            let sentinel = Event::new(LifecycleEvent::Started {
                session_id: "unfenced".into(),
            });
            if matches!(entry, "recv" | "drain") {
                assert_eq!(bus.emit(event.clone()), 1);
                if entry == "recv" {
                    bus.emit(sentinel.clone());
                }
            }
            waits.store(0, Ordering::SeqCst);
            armed.store(true, Ordering::SeqCst);
            match entry {
                "emit" => {
                    assert_eq!(bus.emit(event.clone()), usize::from(!revoke));
                    assert_eq!(
                        events.drain_pending_snapshot(),
                        if revoke { vec![] } else { vec![event] }
                    );
                }
                "recv" => assert_eq!(
                    events.recv().await.unwrap(),
                    if revoke { sentinel } else { event }
                ),
                "drain" => assert_eq!(
                    events.drain_pending_snapshot(),
                    if revoke { vec![] } else { vec![event] }
                ),
                "validator" => {
                    let guards = bus.mutation_validator().acquire(&event);
                    assert_eq!(guards.is_some(), !revoke);
                    drop(guards);
                }
                _ => unreachable!(),
            }
            assert_eq!(held.load(Ordering::SeqCst), 0);
            assert_eq!(busy.load(Ordering::SeqCst), 1, "{entry}, revoke={revoke}");
            assert_eq!(commits.load(Ordering::SeqCst), 1);
            assert_eq!(waits.load(Ordering::SeqCst), 1);
        }
    }
}

#[cfg(unix)]
#[test]
fn host_probes_observe_turns_and_handoff_after_reusing_the_reader() {
    let directory = tempfile::tempdir().expect("directory");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let first =
        OwnerHost::open(directory.path(), quiet_ownership_settings(), bus.clone()).expect("first");
    let second =
        OwnerHost::open(directory.path(), quiet_ownership_settings(), bus).expect("second");
    let old = first.start("thread").expect("start");
    for _ in 0..128 {
        assert_eq!(first.owned_permit("thread").expect("permit"), old);
        assert_eq!(second.attach("thread").expect("observer").lease, old.lease);
        assert!(!first.has_active_turns().expect("idle"));
    }
    old.begin_turn().expect("begin turn");
    assert!(first.has_active_turns().expect("active turn"));
    assert!(
        second
            .attach("thread")
            .expect("active observer")
            .active_turn
    );
    old.checkpoint(&[]).expect("checkpoint");
    assert!(!first.has_active_turns().expect("checkpointed"));

    let next = first.handoff(&old, &second).expect("handoff");
    assert_eq!(first.attach("thread").expect("new owner").lease, next.lease);
    assert!(first.owned_permit("thread").is_err());
    assert_eq!(second.owned_permit("thread").expect("new permit"), next);
    assert!(old.validate_generation().is_err());
    assert!(old.begin_turn().is_err());
    next.begin_turn().expect("new turn");
    next.validate_mutation().expect("new authority");
    assert!(old.validate_mutation().is_err());
    next.checkpoint(&[]).expect("new checkpoint");
}

#[cfg(unix)]
#[test]
fn repeated_host_and_permit_probes_do_not_repair_or_write_schema() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let host = OwnerHost::open(directory.path(), quiet_ownership_settings(), bus).expect("host");
    let permit = host.start("thread").expect("start");
    permit.begin_turn().expect("begin turn");
    let connection = rusqlite::Connection::open(&path).expect("connection");
    // An absent unrelated table makes any accidental schema initialization observable,
    // including CREATE IF NOT EXISTS statements that normally leave no write behind.
    connection
        .execute_batch("DROP TABLE owner_checkpoints")
        .expect("remove checkpoint table");

    for _ in 0..128 {
        host.attach("thread").expect("attach");
        host.owned_permit("thread").expect("permit");
        assert!(host.has_active_turns().expect("active"));
        permit.validate_generation().expect("generation");
        permit.validate_mutation().expect("mutation");
        drop(permit.mutation_guard().expect("guard"));
    }
    let tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'owner_checkpoints'",
            [],
            |row| row.get(0),
        )
        .expect("checkpoint schema");
    assert_eq!(
        tables, 0,
        "read probes must never initialize or repair schema"
    );
    // Schema creation remains an explicit startup responsibility.
    Registry::open(&path).expect("restore schema");
    permit.checkpoint(&[]).expect("checkpoint");
}

#[cfg(unix)]
fn quiet_ownership_settings() -> config::OwnershipConfig {
    config::OwnershipConfig {
        lease_ms: std::num::NonZeroU64::new(u64::MAX).expect("lease"),
        heartbeat_ms: std::num::NonZeroU64::new(u64::MAX).expect("heartbeat"),
        ..config::OwnershipConfig::default()
    }
}

#[cfg(target_os = "linux")]
#[test]
fn repeated_host_display_probes_have_bounded_read_io() {
    fn read_chars() -> u64 {
        std::fs::read_to_string("/proc/thread-self/io")
            .expect("thread I/O counters")
            .lines()
            .find_map(|line| line.strip_prefix("rchar: "))
            .expect("rchar")
            .trim()
            .parse()
            .expect("read byte count")
    }

    let directory = tempfile::tempdir().expect("directory");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let host = OwnerHost::open(directory.path(), quiet_ownership_settings(), bus).expect("host");
    host.start("thread").expect("start");
    host.attach("thread").expect("prime page cache");
    host.owned_permit("thread").expect("prime permit query");
    host.has_active_turns().expect("prime list query");

    let before = read_chars();
    for _ in 0..1_024 {
        host.attach("thread").expect("attach");
        host.owned_permit("thread").expect("permit");
        assert!(!host.has_active_turns().expect("idle"));
    }
    let bytes = read_chars().saturating_sub(before);
    // Includes reads served by the kernel page cache. Reopening SQLite for each
    // probe rereads the schema/pages and exceeds this budget by several MiB.
    // Per-thread counters exclude the background owner worker and parallel tests.
    assert!(bytes <= 256 * 1_024, "display probes read {bytes} bytes");
}

#[cfg(unix)]
#[tokio::test]
async fn deferred_quiesce_keeps_generation_valid_through_checkpoint_and_heartbeats() {
    let directory = tempfile::tempdir().unwrap();
    let bus = std::sync::Arc::new(event_bus::EventBus::new(64));
    let settings = config::OwnershipConfig {
        heartbeat_ms: std::num::NonZeroU64::new(10).unwrap(),
        ..Default::default()
    };
    let host = OwnerHost::open(directory.path(), settings, bus.clone()).unwrap();
    let permit = host.start("thread").unwrap();
    permit.begin_turn().unwrap();
    assert!(host.begin_quiesce().unwrap());
    assert!(permit.begin_turn().is_err());
    permit.checkpoint(&[]).unwrap();
    let mut receiver = bus.subscribe();
    loop {
        let event = match receiver.recv().await {
            Ok(event) => event,
            Err(event_bus::RecvError::Lagged(_)) => continue,
            Err(error) => panic!("ownership events ended: {error:?}"),
        };
        if matches!(event.kind, event_bus::EventKind::Ownership(event)
            if event.thread_id == "thread" && event.action == event_bus::OwnershipAction::Heartbeat)
        {
            break;
        }
    }
    assert_eq!(host.attach("thread").unwrap().state, OwnerState::Quiescing);
    permit.validate_generation().unwrap();
    assert!(!host.begin_quiesce().unwrap());
    assert!(!host.release_ready().unwrap());
    assert_eq!(host.attach("thread").unwrap().state, OwnerState::Released);
    assert!(permit.validate_generation().is_err());
}

#[test]
fn delayed_turn_and_child_start_renew_the_current_generation() {
    let mut idle = owner();
    let token = idle.lease.clone();
    idle.begin_turn(&token, 20_000).expect("delayed turn");
    assert!(idle.active_turn);
    assert_eq!(idle.lease.generation, token.generation);
    assert_eq!(idle.lease.expires_at, 25_000);

    let mut running = owner();
    running.begin_run(&token, "parent", 1).expect("parent");
    running
        .begin_run(&token, "child", 20_000)
        .expect("delayed child");
    assert_eq!(
        running.active_runs,
        ["parent".into(), "child".into()].into()
    );
    assert_eq!(running.lease.generation, token.generation);
    assert_eq!(running.lease.expires_at, 25_000);
}

#[test]
fn expired_permit_recovers_active_work_before_the_heartbeat_worker() {
    for run_id in [None, Some("parent")] {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("owners.db");
        let mut original = owner();
        let token = original.lease.clone();
        if let Some(run) = run_id {
            original.begin_run(&token, run, 1).expect("parent");
            original.begin_run(&token, "child", 1).expect("child");
        } else {
            original.begin_turn(&token, 1).expect("turn");
        }
        let mut registry = Registry::open(&path).expect("registry");
        registry.start(&original).expect("start");
        let permit = OwnerPermit {
            registry_path: path,
            thread_id: original.thread_id.clone(),
            lease: token.clone(),
            run_id: run_id.map(str::to_owned),
        };

        // The entire lease and grace period passed while this process was busy.
        permit
            .validate_mutation_with_clock(|| 20_000)
            .expect("same generation must recover");
        let renewed = registry.attach(&original.thread_id).expect("renewed owner");
        assert_eq!(renewed.lease.expires_at, 25_000);
        assert_eq!(renewed.lease.generation, token.generation);
        assert_eq!(renewed.active_runs, original.active_runs);
        assert!(renewed.active_turn);
        assert!(matches!(
            registry.update(&original.thread_id, |owner| {
                owner.claim(&token, "next", 20_000, owner.settings.grace_ms.get())
            }),
            Err(RegistryError::Ownership(OwnershipError::NotClaimable))
        ));
        // A subsequent healthy probe stays read-only and cannot contend with
        // another connection holding the reserved write lock.
        let writer = rusqlite::Connection::open(&permit.registry_path).expect("writer");
        writer.execute_batch("BEGIN IMMEDIATE").expect("write lock");
        permit
            .validate_mutation_with_clock(|| 20_001)
            .expect("read-only healthy probe");
        writer.execute_batch("ROLLBACK").expect("unlock");
    }
}

#[test]
fn expired_permit_rechecks_a_claim_committed_after_its_read() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut original = owner();
    let token = original.lease.clone();
    original.begin_run(&token, "parent", 1).expect("parent");
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&original).expect("start");
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: original.thread_id.clone(),
        lease: token.clone(),
        run_id: Some("parent".into()),
    };
    let claimed = std::cell::Cell::new(false);
    let result = permit.validate_mutation_with_clock(|| {
        // Interleave a committed claim between the initial read and renewal.
        if !claimed.replace(true) {
            Registry::open_existing(&path)
                .expect("claimant")
                .update(&original.thread_id, |owner| {
                    owner.claim(&token, "next", 20_000, owner.settings.grace_ms.get())
                })
                .expect("claim wins first");
        }
        20_000
    });
    assert!(matches!(
        result,
        Err(RegistryError::Ownership(OwnershipError::Fenced))
    ));
    assert!(matches!(
        registry.update(&original.thread_id, |owner| owner.heartbeat(&token, 20_000)),
        Err(RegistryError::Ownership(OwnershipError::Fenced))
    ));
    let successor = registry.attach(&original.thread_id).expect("successor");
    assert_eq!(successor.lease.owner_id, "next");
    assert_eq!(successor.lease.generation, token.generation + 1);
    assert!(!successor.active_turn);
    assert!(successor.active_runs.is_empty());
}

#[test]
fn expired_permit_cannot_revive_a_run_checkpointed_after_its_read() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut original = owner();
    let token = original.lease.clone();
    original.begin_run(&token, "parent", 1).expect("parent");
    original.begin_run(&token, "child", 1).expect("child");
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&original).expect("start");
    let permit = OwnerPermit {
        registry_path: path,
        thread_id: original.thread_id.clone(),
        lease: token,
        run_id: Some("parent".into()),
    };
    let checkpointed = std::cell::Cell::new(false);
    let result = permit.validate_mutation_with_clock(|| {
        if !checkpointed.replace(true) {
            permit.checkpoint(&[]).expect("checkpoint wins first");
        }
        20_000
    });
    assert!(matches!(
        result,
        Err(RegistryError::Ownership(OwnershipError::Active))
    ));
    let owner = registry
        .attach(&original.thread_id)
        .expect("checkpointed owner");
    assert_eq!(owner.lease, original.lease);
    assert_eq!(owner.active_runs, ["child".into()].into());
    assert!(owner.active_turn);
}

#[test]
fn quiescing_runs_can_recover_and_drain_without_reviving_released_owners() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut original = owner();
    let token = original.lease.clone();
    original.begin_run(&token, "parent", 1).expect("parent");
    original.begin_run(&token, "child", 1).expect("child");
    original.quiesce(&token).expect("quiesce");
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&original).expect("start");
    let permit = OwnerPermit {
        registry_path: path,
        thread_id: original.thread_id.clone(),
        lease: token.clone(),
        run_id: Some("parent".into()),
    };

    permit
        .validate_mutation_with_clock(|| 20_000)
        .expect("drain after delay");
    assert_eq!(
        registry.attach(&original.thread_id).unwrap().state,
        OwnerState::Quiescing
    );
    assert!(matches!(
        registry.update(&original.thread_id, |owner| owner
            .begin_run(&token, "new", 30_000)),
        Err(RegistryError::Ownership(OwnershipError::Quiescing))
    ));
    permit.checkpoint(&[]).expect("parent checkpoint");
    let checkpointed = registry.attach(&original.thread_id).unwrap();
    assert!(matches!(
        permit.validate_mutation_with_clock(|| 30_000),
        Err(RegistryError::Ownership(OwnershipError::Active))
    ));
    assert_eq!(registry.attach(&original.thread_id).unwrap(), checkpointed);
    let child = OwnerPermit {
        run_id: Some("child".into()),
        ..permit.clone()
    };
    child
        .validate_mutation_with_clock(|| 30_000)
        .expect("child drains");
    child.checkpoint(&[]).expect("child checkpoint");
    registry
        .update(&original.thread_id, |owner| owner.release(&token))
        .expect("release");
    let released = registry.attach(&original.thread_id).unwrap();
    assert!(child.validate_mutation_with_clock(|| 40_000).is_err());
    assert!(
        registry
            .update(&original.thread_id, |owner| owner.heartbeat(&token, 40_000))
            .is_err()
    );
    assert_eq!(registry.attach(&original.thread_id).unwrap(), released);
}
