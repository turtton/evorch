use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};

#[test]
fn released_owner_cannot_overwrite_checkpoint() {
    let mut owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    let token = owner.lease.clone();
    owner.quiesce(&token).expect("quiesce");
    owner.release(&token).expect("release");
    assert!(owner.checkpoint(&token).is_err());
}

#[cfg(unix)]
#[test]
fn handoff_preserves_checkpoint_and_applies_successor_settings() {
    let directory = tempfile::tempdir().expect("directory");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let first = runtime::ownership::OwnerHost::open(
        directory.path(),
        config::OwnershipConfig::default(),
        bus.clone(),
    )
    .expect("first");
    let settings = config::OwnershipConfig {
        lease_ms: std::num::NonZeroU64::new(37_000).expect("lease"),
        heartbeat_ms: std::num::NonZeroU64::new(3_000).expect("heartbeat"),
        grace_ms: std::num::NonZeroU64::new(7_000).expect("grace"),
    };
    let second = runtime::ownership::OwnerHost::open(directory.path(), settings.clone(), bus)
        .expect("second");
    let old = first.start("thread").expect("start");
    old.begin_turn().expect("begin");
    assert!(first.handoff(&old, &second).is_err());
    old.checkpoint(&[]).expect("checkpoint");
    let next = first.handoff(&old, &second).expect("handoff");
    assert_eq!(next.lease.generation, old.lease.generation + 1);
    assert!(old.begin_turn().is_err());
    assert_eq!(second.attach("thread").expect("attach").settings, settings);
    next.begin_turn().expect("next turn");
    next.checkpoint(&[]).expect("next checkpoint");
}

#[test]
fn heartbeat_and_claim_use_persisted_lease_settings() {
    let mut owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    owner.settings.lease_ms = std::num::NonZeroU64::new(700).expect("lease");
    owner.settings.grace_ms = std::num::NonZeroU64::new(50).expect("grace");
    let token = owner.lease.clone();
    owner.heartbeat(&token, 120).expect("suspect heartbeat");
    assert_eq!(owner.lease.expires_at, 820);
    assert!(owner.heartbeat(&token, 870).is_err());
    assert!(owner.claim(&token, "next", 869, 50).is_err());
    owner.claim(&token, "next", 870, 50).expect("claim");
    assert_eq!(owner.lease.expires_at, 1_570);
}

#[cfg(unix)]
#[test]
fn ipc_heartbeat_uses_owner_settings() {
    use runtime::ownership::ipc::{Request, Response, request, serve_connection};
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let socket = directory.path().join("owner.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("listener");
    let mut owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    owner.settings.lease_ms = std::num::NonZeroU64::new(700).expect("lease");
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&owner).expect("start");
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        serve_connection(&mut stream, &mut registry, 20).expect("serve");
    });
    let response = request(
        &socket,
        &Request::Heartbeat {
            thread_id: "thread".into(),
            token: owner.lease,
        },
    )
    .expect("heartbeat");
    assert!(matches!(response, Response::Status(owner) if owner.lease.expires_at == 720));
    worker.join().expect("worker");
}

#[cfg(unix)]
#[test]
fn idle_quiesce_creates_checkpoint_before_release() {
    let directory = tempfile::tempdir().expect("directory");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(16));
    let host = runtime::ownership::OwnerHost::open(
        directory.path(),
        config::OwnershipConfig::default(),
        bus,
    )
    .expect("host");
    host.start("thread").expect("start");
    assert!(!host.quiesce().expect("quiesce"));
    let connection = rusqlite::Connection::open(directory.path().join("owners.db")).expect("db");
    let payload: String = connection
        .query_row(
            "SELECT payload FROM owner_checkpoints WHERE thread_id='thread'",
            [],
            |row| row.get(0),
        )
        .expect("checkpoint");
    assert_eq!(payload, "[]");
    assert_eq!(
        host.attach("thread").expect("owner").state,
        runtime::ownership::OwnerState::Released
    );
}

#[test]
fn idle_checkpoint_persists_transcript() {
    // Given: a thread with no active turn.
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    Registry::open(&path)
        .expect("registry")
        .start(&owner)
        .expect("start");
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: owner.thread_id,
        lease: owner.lease,
        run_id: None,
    };
    // When: idle work is checkpointed before shutdown.
    permit.checkpoint(&[]).expect("checkpoint");
    // Then: a durable checkpoint exists even without a running turn.
    let connection = rusqlite::Connection::open(path).expect("connection");
    let payload: String = connection
        .query_row(
            "SELECT payload FROM owner_checkpoints WHERE thread_id='thread'",
            [],
            |row| row.get(0),
        )
        .expect("checkpoint row");
    assert_eq!(payload, "[]");
}

#[test]
fn stale_claim_clears_abandoned_child_runs() {
    // Given: an owner crashed with multiple active runs.
    let mut owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    let old = owner.lease.clone();
    owner.begin_run(&old, "parent", 1).expect("parent");
    owner.begin_run(&old, "child", 1).expect("child");
    // When: liveness was checked and the grace period has elapsed.
    owner.claim(&old, "next", 150, 50).expect("claim");
    // Then: old runs cannot checkpoint or block the successor.
    assert!(owner.active_runs.is_empty());
    assert!(!owner.active_turn);
    assert!(owner.checkpoint_run(&old, "child").is_err());
    owner
        .begin_run(&owner.lease.clone(), "next-run", 150)
        .expect("new run");
}

#[test]
fn released_permit_rejects_mutations_before_successor_claims() {
    // Given: a released owner whose generation has not changed yet.
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: owner.thread_id.clone(),
        lease: owner.lease.clone(),
        run_id: None,
    };
    owner.quiesce(&permit.lease).expect("quiesce");
    owner.release(&permit.lease).expect("release");
    Registry::open(&path)
        .expect("registry")
        .start(&owner)
        .expect("start");
    // When / Then: neither command validation nor a mutation guard accepts it.
    assert!(permit.validate_generation().is_err());
    assert!(permit.mutation_guard().is_err());
}

#[cfg(unix)]
#[test]
fn ipc_idle_quiesce_checkpoints_without_waiting_for_heartbeat() {
    use runtime::ownership::ipc::{Request, Response, request, serve_connection};
    // Given: an idle owner served over IPC, without a heartbeat worker.
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let socket = directory.path().join("owner.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("listener");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&owner).expect("start");
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        serve_connection(&mut stream, &mut registry, 20).expect("serve");
    });
    // When: quiesce is requested with no active turn.
    let response = request(
        &socket,
        &Request::Quiesce {
            thread_id: "thread".into(),
            token: owner.lease,
        },
    )
    .expect("quiesce");
    worker.join().expect("worker");
    // Then: release and its durable checkpoint are already committed.
    assert!(
        matches!(response, Response::Status(owner) if owner.state == runtime::ownership::OwnerState::Released)
    );
    let connection = rusqlite::Connection::open(path).expect("db");
    let payload: String = connection
        .query_row(
            "SELECT payload FROM owner_checkpoints WHERE thread_id='thread'",
            [],
            |row| row.get(0),
        )
        .expect("checkpoint");
    assert_eq!(payload, "[]");
}
