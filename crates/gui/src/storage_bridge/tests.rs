use super::*;
use event_bus::{LifecycleEvent, OwnershipAction, OwnershipEvent, UsageEvent};
use std::time::{Duration, UNIX_EPOCH};
use storage::{Database, Storage, StorageConfig};

fn fixture() -> (tempfile::TempDir, Storage, Database) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    (dir, storage, db)
}

fn usage_event(tokens: u64) -> Event {
    let mut event = Event::new(UsageEvent::Usage {
        provider: "provider".into(),
        model: "model".into(),
        input_tokens: tokens,
        output_tokens: 2,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
    });
    event.meta.wall_clock = UNIX_EPOCH + Duration::from_secs(125);
    event
}

#[test]
fn usage_event_is_not_persisted_raw() {
    // Given: a real storage writer.
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    // When: usage reaches the bridge.
    let result = bridge.handle_event(&usage_event(10));
    // Then: raw usage is accepted but never persisted.
    assert!(result.is_ok(), "{result:?}");
    assert!(db.events_all_ordered().unwrap().is_empty());
}

#[test]
fn lifecycle_event_is_persisted() {
    // Given: a session lifecycle event.
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let event = Event::new(LifecycleEvent::Started {
        session_id: "session".into(),
    });
    // When: it reaches the bridge.
    bridge.handle_event(&event).unwrap();
    // Then: the event remains available for replay.
    let events = db.events_all_ordered().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event);
}

#[test]
fn ledger_event_is_persisted() {
    // Given: a ledger event and a real storage writer.
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let event = Event::new(event_bus::LedgerEvent::RunLedgerAppended {
        run_id: "run-1".into(),
        seq: 7,
        body: "entry".into(),
    });
    // When: the ledger event reaches the bridge.
    bridge.handle_event(&event).unwrap();
    // Then: the complete event is available for replay in its session.
    let events = db.events_all_ordered().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session_id.as_deref(), Some("session"));
    assert_eq!(events[0].event, event);
}

#[test]
fn ownership_event_is_persisted() {
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let event = Event::new(OwnershipEvent {
        thread_id: "thread-1".into(),
        owner_id: "owner-1".into(),
        generation: 2,
        action: OwnershipAction::Quiescing,
    });

    bridge.handle_event(&event).unwrap();

    let events = db.events_all_ordered().unwrap();
    assert_eq!(
        events,
        vec![storage::StoredEvent {
            id: events[0].id,
            session_id: Some("session".into()),
            event,
        }]
    );
}

#[test]
fn flush_usage_produces_metrics_bucket() {
    // Given: two observations in one minute.
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    bridge.handle_event(&usage_event(10)).unwrap();
    bridge.handle_event(&usage_event(20)).unwrap();
    // When: usage is flushed twice (the second flush must be empty).
    bridge.flush_usage();
    bridge.flush_usage();
    storage.handle().flush_usage_now().unwrap();
    // Then: one additive bucket, without duplicate accounting.
    let metrics = db.metrics_range(120, 180).unwrap();
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].input_tokens, 30);
    assert_eq!(metrics[0].output_tokens, 4);
    assert_eq!(metrics[0].cache_read_tokens, 6);
    assert_eq!(metrics[0].cache_write_tokens, 8);
    assert_eq!(metrics[0].request_count, 2);
}

#[test]
fn live_shell_output_is_not_persisted() {
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    bridge
        .handle_event(&Event::new(event_bus::ToolEvent::ShellJobOutput {
            job_id: "job".into(),
            call_id: None,
            run_id: Some("run".into()),
            offset: 0,
            chunk: "line\n".into(),
            status: "running".into(),
            exit_code: None,
        }))
        .unwrap();
    assert!(db.events_all_ordered().unwrap().is_empty());
}

#[test]
fn heartbeat_is_observational_but_ownership_transitions_are_durable() {
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let actions = [
        OwnershipAction::Claimed,
        OwnershipAction::Suspect,
        OwnershipAction::Stale,
        OwnershipAction::Claimable,
        OwnershipAction::Quiescing,
        OwnershipAction::Released,
        OwnershipAction::Handoff,
        OwnershipAction::MutationRejected,
    ];
    for _ in 0..1_000 {
        bridge
            .handle_event(&Event::new(OwnershipEvent {
                thread_id: "thread".into(),
                owner_id: "owner".into(),
                generation: 1,
                action: OwnershipAction::Heartbeat,
            }))
            .unwrap();
    }
    assert!(db.events_all_ordered().unwrap().is_empty());
    for action in actions {
        bridge
            .handle_event(&Event::new(OwnershipEvent {
                thread_id: "thread".into(),
                owner_id: "owner".into(),
                generation: 1,
                action,
            }))
            .unwrap();
    }
    assert_eq!(db.events_all_ordered().unwrap().len(), actions.len());
    assert_eq!(bridge.monitor().snapshot().skipped_heartbeats, 1_000);
    assert_eq!(
        bridge.monitor().snapshot().persisted_events,
        actions.len() as u64
    );
}

#[test]
fn diagnostics_policy_preserves_warnings_errors_and_sandbox_audit() {
    use event_bus::{DiagnosticEvent, DiagnosticSeverity};
    for (policy, expected) in [
        (DiagnosticPersistence::Off, 1),
        (DiagnosticPersistence::Warnings, 3),
        (DiagnosticPersistence::All, 4),
    ] {
        let (_dir, storage, db) = fixture();
        let mut bridge =
            StorageBridge::new(storage.handle(), "session").with_diagnostic_persistence(policy);
        for severity in [
            DiagnosticSeverity::Info,
            DiagnosticSeverity::Warning,
            DiagnosticSeverity::Error,
        ] {
            bridge
                .handle_event(&Event::new(DiagnosticEvent {
                    source: "test".into(),
                    severity,
                    code: "observation".into(),
                    detail: "detail".into(),
                    run_id: Some("run".into()),
                    thread_id: None,
                    call_id: None,
                }))
                .unwrap();
        }
        bridge
            .handle_event(&Event::new(DiagnosticEvent {
                source: "sandbox".into(),
                severity: DiagnosticSeverity::Info,
                code: "escalation_review".into(),
                detail: "audit".into(),
                run_id: Some("run".into()),
                thread_id: None,
                call_id: None,
            }))
            .unwrap();
        assert_eq!(db.events_all_ordered().unwrap().len(), expected);
        assert_eq!(
            bridge.monitor().snapshot().skipped_diagnostics,
            (4 - expected) as u64
        );
    }
}

#[test]
fn metrics_can_be_disabled_without_disabling_conversation_persistence() {
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session").with_metrics_enabled(false);
    bridge.handle_event(&usage_event(10)).unwrap();
    bridge.flush_usage();
    let lifecycle = Event::new(LifecycleEvent::Started {
        session_id: "session".into(),
    });
    bridge.handle_event(&lifecycle).unwrap();
    assert!(db.metrics_range(120, 180).unwrap().is_empty());
    assert_eq!(db.events_all_ordered().unwrap()[0].event, lifecycle);
}

#[tokio::test(start_paused = true)]
async fn a_single_delta_flushes_on_deadline_and_shutdown_drains_the_tail() {
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let monitor = bridge.monitor();
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let task = spawn_test_bridge_until_shutdown(&bus, bridge, Duration::from_secs(3600), async {
        shutdown.await.unwrap();
    });

    let event = Event::new(event_bus::MessageEvent::MessageDelta {
        run_id: Some("run".into()),
        delta: "deadline".into(),
    });
    bus.emit(event.clone());
    monitor.wait_for(|state| state.pending_events == 1).await;
    assert_eq!(monitor.snapshot().persisted_events, 0);
    tokio::time::advance(COALESCE_INTERVAL).await;
    monitor.wait_for(|state| state.persisted_events == 1).await;
    assert_eq!(db.events_all_ordered().unwrap()[0].event, event);
    bus.emit(Event::new(event_bus::MessageEvent::MessageDelta {
        run_id: Some("run".into()),
        delta: "tail".into(),
    }));
    // The blocking SQLite worker keeps the paused runtime from automatically
    // advancing timers. Signal shutdown explicitly rather than waiting for a
    // future tick to discover that the last producer was dropped.
    stop.send(()).unwrap();
    task.await.unwrap();
    let events = db.events_all_ordered().unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[1].event.kind,
        EventKind::Message(event_bus::MessageEvent::MessageDelta { delta, .. }) if delta == "tail"));
    assert_eq!(monitor.snapshot().pending_events, 0);
    assert_eq!(monitor.snapshot().oldest_pending_age, Duration::ZERO);
}

#[tokio::test]
async fn coalesced_delta_still_checks_the_generation_at_storage_write() {
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let bus = EventBus::new(32);
    let accepted = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = accepted.clone();
    bus.register_mutation_fence(
        "run".into(),
        Arc::new(move || flag.load(std::sync::atomic::Ordering::SeqCst)),
    );
    bridge.validator = Some(bus.mutation_validator());
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let mut queue = EventQueue::new(
        tx,
        bridge.monitor(),
        PersistencePolicy::default(),
        storage.handle().max_event_bytes(),
    );
    for text in ["first", "second"] {
        queue
            .push(Event::new(event_bus::MessageEvent::MessageDelta {
                run_id: Some("run".into()),
                delta: text.into(),
            }))
            .await
            .unwrap();
    }
    queue.flush().await.unwrap();
    accepted.store(false, std::sync::atomic::Ordering::SeqCst);
    let WriteRequest::Event(queued) = rx.recv().await.unwrap() else {
        panic!("expected event")
    };
    assert!(matches!(
        bridge.handle_event(&queued.event),
        Err(StorageError::StaleMutation)
    ));
    assert!(db.events_all_ordered().unwrap().is_empty());
    assert_eq!(bridge.monitor().snapshot().persisted_events, 0);
    assert_eq!(bridge.monitor().snapshot().failed_events, 1);
}

#[tokio::test]
async fn filtering_affects_only_durable_rows_and_leaves_live_observations_intact() {
    use event_bus::{DiagnosticEvent, DiagnosticSeverity};
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(256));
    let mut live = bus.subscribe();
    let bridge = StorageBridge::new(storage.handle(), "session").with_metrics_enabled(false);
    let monitor = bridge.monitor();
    let task = spawn_test_bridge(&bus, bridge, Duration::from_millis(10));

    let heartbeat = Event::new(OwnershipEvent {
        thread_id: "thread".into(),
        owner_id: "owner".into(),
        generation: 1,
        action: OwnershipAction::Heartbeat,
    });
    let info = Event::new(DiagnosticEvent {
        source: "test".into(),
        severity: DiagnosticSeverity::Info,
        code: "observation".into(),
        detail: "detail".into(),
        run_id: None,
        thread_id: None,
        call_id: None,
    });
    for _ in 0..100 {
        bus.emit(heartbeat.clone());
        assert_eq!(live.recv().await.unwrap(), heartbeat);
    }
    for event in [info, usage_event(10)] {
        bus.emit(event.clone());
        assert_eq!(live.recv().await.unwrap(), event);
    }
    drop(bus);
    task.await.unwrap();
    assert!(db.events_all_ordered().unwrap().is_empty());
    assert!(db.metrics_range(120, 180).unwrap().is_empty());
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.skipped_heartbeats, 100);
    assert_eq!(snapshot.skipped_diagnostics, 1);
    assert_eq!(snapshot.peak_pending_events, 0);
    assert_eq!(snapshot.persisted_events, 0);
}

#[tokio::test]
async fn real_owner_handoff_still_rejects_an_already_queued_old_generation() {
    let (dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(64));
    let root = dir.path().join("owners");
    let first =
        runtime::ownership::OwnerHost::open(&root, Default::default(), bus.clone()).unwrap();
    let successor =
        runtime::ownership::OwnerHost::open(&root, Default::default(), bus.clone()).unwrap();
    let permit = first.start("thread").unwrap();
    let token = permit.clone();
    bus.register_mutation_guard(
        "run".into(),
        Arc::new(move || {
            token
                .mutation_guard()
                .ok()
                .map(|guard| Box::new(guard) as Box<dyn event_bus::MutationGuard>)
        }),
    );
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    bridge.validator = Some(bus.mutation_validator());
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let mut queue = EventQueue::new(
        tx,
        bridge.monitor(),
        PersistencePolicy::default(),
        storage.handle().max_event_bytes(),
    );
    queue
        .push(Event::new(event_bus::MessageEvent::MessageDelta {
            run_id: Some("run".into()),
            delta: "queued by old owner".into(),
        }))
        .await
        .unwrap();
    first.begin_quiesce().unwrap();
    let next = first.handoff(&permit, &successor).unwrap();
    assert!(next.lease.generation > permit.lease.generation);
    queue.flush().await.unwrap();
    let WriteRequest::Event(queued) = rx.recv().await.unwrap() else {
        panic!("expected delta")
    };
    assert!(matches!(
        bridge.handle_event(&queued.event),
        Err(StorageError::StaleMutation)
    ));
    assert!(db.events_all_ordered().unwrap().is_empty());
}

#[test]
fn a_closed_writer_spools_one_halt_per_episode() {
    // Given: a bridge spooling halts, whose storage writer has shut down.
    let (dir, storage, _db) = fixture();
    let spool = dir.path().join("crash-spool");
    let mut bridge =
        StorageBridge::new(storage.handle(), "session").with_fault_spool(spool.clone());
    drop(storage);
    let event = || {
        Event::new(LifecycleEvent::Started {
            session_id: "session".into(),
        })
    };
    // When: several events are refused in a row.
    for _ in 0..3 {
        assert!(matches!(
            bridge.handle_event(&event()),
            Err(StorageError::WriterClosed)
        ));
    }
    // Then: one spooled StorageWriterHalted fault, ingestible on the next start.
    let spooled = runtime::self_improvement::drain_crash_spool(&spool);
    assert_eq!(spooled.len(), 1);
    assert_eq!(spooled[0].code.as_deref(), Some("StorageWriterHalted"));
    assert_eq!(
        spooled[0].location.as_deref(),
        Some("storage:writer_closed")
    );
    assert!(
        spooled[0].message.starts_with("event writes halted:"),
        "{}",
        spooled[0].message
    );
}
