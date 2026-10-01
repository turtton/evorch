use super::*;
use event_bus::{Event, MessageEvent, OwnershipAction, OwnershipEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use storage::{Database, StorageConfig};

fn fixture() -> (tempfile::TempDir, Storage, Database) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("shutdown.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    (dir, storage, db)
}

fn delta(text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some("run".into()),
        delta: text.into(),
    })
}

#[test]
fn owned_shutdown_drains_partial_delta_with_a_continuously_live_producer() {
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(256));
    let owner = OwnedStorageBridge::spawn(
        bus.clone(),
        storage,
        |handle| StorageBridge::new(handle, "session"),
        Duration::from_secs(60),
    )
    .unwrap();
    let monitor = owner.monitor();
    let event = delta("pending at application close");
    assert_eq!(bus.emit(event.clone()), 1);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while monitor.snapshot().pending_events == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "delta must enter the partial batch"
        );
        std::thread::yield_now();
    }
    assert_eq!(monitor.snapshot().persisted_events, 0);
    let running = Arc::new(AtomicBool::new(true));
    let emit = running.clone();
    let producer_bus = bus.clone();
    let producer = std::thread::spawn(move || {
        while emit.load(Ordering::Relaxed) {
            producer_bus.emit(Event::new(OwnershipEvent {
                thread_id: "thread".into(),
                owner_id: "owner".into(),
                generation: 1,
                action: OwnershipAction::Heartbeat,
            }));
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let (closed, completion) = std::sync::mpsc::channel();
    let closer = std::thread::spawn(move || {
        drop(owner);
        let _ = closed.send(());
    });
    let result = completion.recv_timeout(Duration::from_secs(2));
    running.store(false, Ordering::Relaxed);
    producer.join().unwrap();
    result.expect("live producer must not prevent bounded shutdown");
    closer.join().unwrap();
    let stored = db.events_all_ordered().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].event, event);
    assert_eq!(monitor.snapshot().pending_events, 0);
    assert_eq!(monitor.snapshot().failed_events, 0);
    // Producers can still emit, but events after closure are outside its snapshot.
    assert_eq!(bus.emit(delta("after close")), 0);
    assert_eq!(db.events_all_ordered().unwrap(), stored);
}

#[test]
fn startup_error_drops_guard_and_persists_its_snapshot_before_storage_closes() {
    fn fail_after_start(
        bus: Arc<EventBus>,
        storage: Storage,
        event: Event,
    ) -> Result<(), &'static str> {
        let _owner = OwnedStorageBridge::spawn(
            bus.clone(),
            storage,
            |handle| StorageBridge::new(handle, "session"),
            Duration::from_secs(60),
        )
        .unwrap();
        bus.emit(event);
        Err("later startup failure")
    }
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let event = delta("startup tail");
    assert_eq!(
        fail_after_start(bus.clone(), storage, event.clone()),
        Err("later startup failure")
    );
    let stored = db.events_all_ordered().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].event, event);
    assert_eq!(bus.receiver_count(), 0);
}

#[test]
fn explicit_shutdown_is_idempotent_and_leaves_the_writer_available_until_drop() {
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let mut owner = OwnedStorageBridge::spawn(
        bus.clone(),
        storage,
        |handle| StorageBridge::new(handle, "session"),
        Duration::from_secs(60),
    )
    .unwrap();
    bus.emit(delta("tail"));
    owner.shutdown();
    owner.shutdown();
    owner.handle().flush_usage_now().unwrap();
    assert_eq!(db.events_all_ordered().unwrap().len(), 1);
    assert_eq!(bus.receiver_count(), 0);
}

#[test]
fn real_owner_quiesce_preserves_a_queued_delta_until_owned_storage_shutdown() {
    let (dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(64));
    let host = runtime::ownership::OwnerHost::open(
        &dir.path().join("owners"),
        Default::default(),
        bus.clone(),
    )
    .unwrap();
    let permit = host.start("thread").unwrap();
    let fence = permit.clone();
    bus.register_mutation_guard(
        "run".into(),
        Arc::new(move || {
            fence
                .mutation_guard()
                .ok()
                .map(|guard| Box::new(guard) as Box<dyn event_bus::MutationGuard>)
        }),
    );
    let mut owner = OwnedStorageBridge::spawn(
        bus.clone(),
        storage,
        |handle| StorageBridge::new(handle, "session"),
        Duration::from_secs(60),
    )
    .unwrap();
    let event = delta("last generation-fenced delta");
    bus.emit(event.clone());
    let monitor = owner.monitor();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while monitor.snapshot().pending_events == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(monitor.snapshot().persisted_events, 0);
    assert!(!host.begin_quiesce().unwrap());
    assert_eq!(
        host.attach("thread").unwrap().state,
        runtime::ownership::OwnerState::Quiescing
    );
    permit.validate_generation().unwrap();
    owner.flush().unwrap();
    assert!(!host.release_ready().unwrap());
    owner.shutdown();
    assert!(permit.validate_generation().is_err());
    let messages: Vec<_> = db
        .events_all_ordered()
        .unwrap()
        .into_iter()
        .filter(|row| matches!(row.event.kind, event_bus::EventKind::Message(_)))
        .map(|row| row.event)
        .collect();
    assert_eq!(messages, [event]);
    let rows = db.events_all_ordered().unwrap();
    let delta_index = rows
        .iter()
        .position(|row| matches!(&row.event.kind, event_bus::EventKind::Message(_)))
        .unwrap();
    let released_index = rows
        .iter()
        .position(|row| {
            matches!(&row.event.kind,
        event_bus::EventKind::Ownership(owner) if owner.action == OwnershipAction::Released)
        })
        .expect("release is persisted before subscriber exits");
    assert!(delta_index < released_index);
    assert_eq!(monitor.snapshot().failed_events, 0);
}

#[test]
fn flush_acknowledges_durable_rows_without_stopping_the_subscriber() {
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let mut owner = OwnedStorageBridge::spawn(
        bus.clone(),
        storage,
        |handle| StorageBridge::new(handle, "session"),
        Duration::from_secs(60),
    )
    .unwrap();
    let before = delta("before barrier");
    bus.emit(before.clone());
    owner.flush().unwrap();
    assert_eq!(db.events_all_ordered().unwrap()[0].event, before);
    assert_eq!(owner.monitor().snapshot().pending_events, 0);
    assert_eq!(bus.receiver_count(), 1);
    let after = delta("after barrier");
    bus.emit(after.clone());
    owner.shutdown();
    let rows = db.events_all_ordered().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].event, after);
    assert_eq!(
        owner.flush().unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
}

#[test]
fn flush_barrier_reports_a_rejected_write_instead_of_acknowledging_success() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(StorageConfig {
        db_path: dir.path().join("rejected.db"),
        hard_limits: storage::HardLimits {
            max_event_bytes: 0,
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(32));
    let mut owner = OwnedStorageBridge::spawn(
        bus.clone(),
        storage,
        |handle| StorageBridge::new(handle, "session"),
        Duration::from_secs(60),
    )
    .unwrap();
    bus.emit(delta("rejected"));
    let error = owner.flush().unwrap_err();
    assert!(error.to_string().contains("EventSize"), "{error}");
    assert_eq!(owner.monitor().snapshot().failed_events, 1);
    // Failures are reported once per barrier interval, while the bridge stays usable.
    owner.flush().unwrap();
}
