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
#[test]
fn deferred_quiesce_keeps_generation_valid_through_checkpoint_and_heartbeats() {
    let directory = tempfile::tempdir().unwrap();
    let bus = std::sync::Arc::new(event_bus::EventBus::new(64));
    let settings = config::OwnershipConfig {
        heartbeat_ms: std::num::NonZeroU64::new(10).unwrap(),
        ..Default::default()
    };
    let host = OwnerHost::open(directory.path(), settings, bus).unwrap();
    let permit = host.start("thread").unwrap();
    permit.begin_turn().unwrap();
    assert!(host.begin_quiesce().unwrap());
    assert!(permit.begin_turn().is_err());
    permit.checkpoint(&[]).unwrap();
    let initial_expiry = host.attach("thread").unwrap().lease.expires_at;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let owner = host.attach("thread").unwrap();
        assert_eq!(owner.state, OwnerState::Quiescing);
        permit.validate_generation().unwrap();
        if owner.lease.expires_at > initial_expiry {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "heartbeat must continue while release is deferred"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(!host.begin_quiesce().unwrap());
    assert!(!host.release_ready().unwrap());
    assert_eq!(host.attach("thread").unwrap().state, OwnerState::Released);
    assert!(permit.validate_generation().is_err());
}
