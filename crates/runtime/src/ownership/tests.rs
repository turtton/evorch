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
