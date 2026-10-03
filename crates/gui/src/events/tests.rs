use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};

use event_bus::{Event, EventBus, LifecycleEvent};

use super::{Delivery, EventPump, MAX_FRAME_EVENTS, expire_delivery};

fn event(id: impl ToString) -> Event {
    Event::new(LifecycleEvent::Started {
        session_id: id.to_string(),
    })
}

fn pump(runtime: &tokio::runtime::Runtime, bus: &EventBus) -> (EventPump, mpsc::Receiver<()>) {
    let (sender, receiver) = mpsc::channel();
    (
        EventPump::spawn(
            runtime.handle(),
            bus.subscribe(),
            Some(Arc::new(move || {
                let _ = sender.send(());
            })),
        ),
        receiver,
    )
}

fn collect(pump: &mut EventPump, repaint: &mpsc::Receiver<()>, count: usize) -> Vec<Event> {
    let mut events = Vec::new();
    while events.len() < count {
        repaint.recv().expect("repaint");
        let batch = pump.drain();
        assert!(batch.len() <= MAX_FRAME_EVENTS);
        events.extend(batch);
    }
    events
}

#[test]
fn drain_rejects_event_when_generation_changes_after_delivery() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let bus = EventBus::new(8);
    let generation = Arc::new(AtomicUsize::new(1));
    let observed = Arc::clone(&generation);
    assert!(bus.register_mutation_fence(
        "run-1".into(),
        Arc::new(move || observed.load(Ordering::SeqCst) == 1)
    ));
    let (mut pump, repaint) = pump(&runtime, &bus);
    bus.emit(event("run-1"));
    repaint.recv().expect("queued");
    generation.store(2, Ordering::SeqCst);
    let sentinel = event("unfenced");
    bus.emit(sentinel.clone());
    assert_eq!(collect(&mut pump, &repaint, 1), vec![sentinel]);
}

#[test]
fn pump_wakes_and_forwards_ordered_batches_without_loss() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let bus = EventBus::new(MAX_FRAME_EVENTS * 4);
    let (mut pump, repaint) = pump(&runtime, &bus);
    assert!(pump.drain().is_empty());
    let expected: Vec<_> = (0..MAX_FRAME_EVENTS * 2 + 1).map(event).collect();
    for event in &expected {
        bus.emit(event.clone());
    }
    assert_eq!(collect(&mut pump, &repaint, expected.len()), expected);
    assert!(pump.drain().is_empty());
}

#[test]
fn runtime_can_stop_before_its_gui_pump_is_dropped() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let bus = EventBus::new(8);
    let (pump, _) = pump(&runtime, &bus);
    drop(runtime);
    drop(pump);
}

#[test]
fn drain_keeps_running_while_worker_acquires_and_releases_a_guard() {
    struct WorkerGuard {
        gui_thread: std::thread::ThreadId,
        dropped: mpsc::Sender<()>,
    }
    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            assert_ne!(std::thread::current().id(), self.gui_thread);
            self.dropped.send(()).expect("guard released");
        }
    }

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let bus = EventBus::new(8);
    let gui_thread = std::thread::current().id();
    let block = Arc::new(AtomicBool::new(false));
    let block_check = Arc::clone(&block);
    let offthread_only = Arc::new(AtomicBool::new(false));
    let offthread_check = Arc::clone(&offthread_only);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let (drop_tx, drop_rx) = mpsc::channel();
    assert!(bus.register_mutation_guard(
        "run-1".into(),
        Arc::new(move || {
            if offthread_check.load(Ordering::Acquire) {
                assert_ne!(std::thread::current().id(), gui_thread);
            }
            if block_check.swap(false, Ordering::AcqRel) {
                assert_ne!(std::thread::current().id(), gui_thread);
                entered_tx.send(()).expect("validation started");
                release_rx
                    .lock()
                    .unwrap()
                    .recv()
                    .expect("release validation");
                Some(Box::new(WorkerGuard {
                    gui_thread,
                    dropped: drop_tx.clone(),
                }))
            } else {
                Some(Box::new(()))
            }
        })
    ));
    let (mut pump, repaint) = pump(&runtime, &bus);
    let expected = event("run-1");
    bus.emit(expected.clone());
    repaint.recv().expect("queued before validation is blocked");
    offthread_only.store(true, Ordering::Release);
    block.store(true, Ordering::Release);
    assert!(pump.drain().is_empty());
    entered_rx
        .recv()
        .expect("worker blocked inside guard acquisition");
    assert!(pump.drain().is_empty());
    release_tx.send(()).expect("resume worker");
    assert_eq!(collect(&mut pump, &repaint, 1), vec![expected]);
    drop_rx
        .recv()
        .expect("worker dropped the guard after delivery");
}

#[test]
fn queued_event_is_rejected_after_an_external_registry_claim() {
    use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread-1".into(),
        Lease {
            owner_id: "old-owner".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    Registry::open(&path).unwrap().start(&owner).unwrap();
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: owner.thread_id.clone(),
        lease: owner.lease.clone(),
        run_id: None,
    };
    let bus = EventBus::new(8);
    assert!(bus.register_mutation_guard(
        "run-1".into(),
        Arc::new(move || {
            permit
                .mutation_guard()
                .ok()
                .map(|guard| Box::new(guard) as Box<dyn event_bus::MutationGuard>)
        })
    ));
    let (mut pump, repaint) = pump(&runtime, &bus);
    bus.emit(event("run-1"));
    repaint.recv().expect("old-generation event queued");
    Registry::open_existing(&path)
        .unwrap()
        .update("thread-1", |current| {
            current.claim(&owner.lease, "new-owner", 200, 50)
        })
        .expect("claim through an independent SQLite connection");
    let sentinel = event("unfenced");
    bus.emit(sentinel.clone());
    assert_eq!(collect(&mut pump, &repaint, 1), vec![sentinel]);
}

#[test]
fn batch_contention_releases_earlier_sqlite_guards_before_retry() {
    use event_bus::{MutationBatchError, MutationGuardAttempt};
    use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    Registry::open(&path).unwrap().start(&owner).unwrap();
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: owner.thread_id.clone(),
        lease: owner.lease.clone(),
        run_id: None,
    };
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.busy_timeout(std::time::Duration::ZERO).unwrap();
    let writer = Arc::new(Mutex::new(writer));
    let pending_writer = Arc::clone(&writer);
    let first = Arc::new(AtomicBool::new(true));
    let bus = EventBus::new(8);
    for run in ["a", "b"] {
        let token = permit.clone();
        let pending_writer = Arc::clone(&pending_writer);
        let first = Arc::clone(&first);
        assert!(bus.register_nonblocking_mutation_guard(
            run.into(),
            Arc::new(|| panic!("batch acquisition must never use the blocking callback")),
            Arc::new(move || match token.try_mutation_guard() {
                Ok(Some(guard)) => {
                    if run == "a" && first.swap(false, Ordering::AcqRel) {
                        // COMMIT with an existing reader returns BUSY while
                        // retaining the writer's PENDING lock. This explicitly
                        // creates A-reader -> writer -> B-reader inversion.
                        let error = pending_writer
                            .lock()
                            .unwrap()
                            .execute_batch(
                                "BEGIN IMMEDIATE; UPDATE thread_owners SET state = state; COMMIT",
                            )
                            .expect_err("A's guard holds the original read transaction");
                        assert_eq!(
                            error.sqlite_error_code(),
                            Some(rusqlite::ErrorCode::DatabaseBusy)
                        );
                    }
                    MutationGuardAttempt::Acquired(Box::new(guard))
                }
                Ok(None) => MutationGuardAttempt::Busy,
                Err(error) => panic!("unexpected guard rejection: {error}"),
            }),
        ));
    }
    let compound = Event::new(LifecycleEvent::EscalationRequested {
        source_run_id: "a".into(),
        new_run_id: "b".into(),
        summary: event_bus::EscalationMemoSummary {
            original_request: String::new(),
            escalation_reason: String::new(),
            files_touched: Vec::new(),
            blockers: Vec::new(),
            suggested_next: String::new(),
        },
    });
    let validator = bus.mutation_validator();
    for events in [vec![event("a"), event("b")], vec![compound]] {
        first.store(true, Ordering::Release);
        assert!(matches!(
            validator.acquire_batch(&events),
            Err(MutationBatchError::Busy)
        ));
        // If A remained held, this immediate COMMIT would still return BUSY.
        writer
            .lock()
            .unwrap()
            .execute_batch("COMMIT")
            .expect("all partial guards released");
        let guards = validator
            .acquire_batch(&events)
            .expect("writer committed before retry");
        assert_eq!(guards.accepted(), vec![true; events.len()]);
    }
}

#[test]
fn busy_batches_retry_without_losing_or_reordering_legacy_events() {
    use event_bus::MutationGuardAttempt;

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let bus = EventBus::new(8);
    let attempts = Arc::new(AtomicUsize::new(0));
    let checked = Arc::clone(&attempts);
    assert!(bus.register_nonblocking_mutation_guard(
        "retry".into(),
        Arc::new(|| Some(Box::new(()))),
        Arc::new(move || if checked.fetch_add(1, Ordering::Relaxed) == 0 {
            MutationGuardAttempt::Busy
        } else {
            MutationGuardAttempt::Acquired(Box::new(()))
        }),
    ));
    assert!(bus.register_mutation_guard("legacy".into(), Arc::new(|| Some(Box::new(())))));
    let (mut pump, repaint) = pump(&runtime, &bus);
    let events = vec![
        event("retry"),
        event("legacy"),
        event("retry"),
        event("unfenced"),
    ];
    for event in &events {
        bus.emit(event.clone());
    }
    assert_eq!(collect(&mut pump, &repaint, events.len()), events);
    assert!(attempts.load(Ordering::Relaxed) >= 2);
}

#[test]
fn expired_guard_handoff_requires_revalidation_without_losing_events() {
    let events = vec![event("first"), event("second")];
    let mut delivery = Delivery::Ready {
        events: events.clone(),
        remaining: Vec::new(),
        revision: 0,
    };
    assert!(expire_delivery(&mut delivery));
    match &delivery {
        Delivery::Retry(pending) => assert_eq!(pending, &events),
        _ => panic!("expired guards must never remain ready"),
    }
    assert!(!expire_delivery(&mut delivery));
}
