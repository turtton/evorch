//! The bridge end to end: bus events through the queue into a real database.

use super::*;
use event_bus::{
    ContextComposition, EventBus, FaultEvent, LifecycleEvent, MessageEvent, RunActivity,
};
use storage::{Database, Storage, StorageConfig, StoredEvent};

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

fn reasoning_of<'a>(rows: &'a [StoredEvent], run: &str) -> Vec<(&'a StoredEvent, &'a str)> {
    rows.iter()
        .filter_map(|row| match &row.event.kind {
            EventKind::Message(MessageEvent::ReasoningDelta {
                run_id: Some(id),
                delta,
            }) if id == run => Some((row, delta.as_str())),
            _ => None,
        })
        .collect()
}

fn completed_id(rows: &[StoredEvent], run: &str) -> i64 {
    rows.iter()
        .find(|row| {
            matches!(&row.event.kind,
                EventKind::Message(MessageEvent::MessageCompleted { run_id, .. }) if run_id == run)
        })
        .map(|row| row.id)
        .expect("MessageCompleted row")
}

// Paused time never advances on its own while the bridge's blocking writer is
// alive, so the 5 s age limit cannot split rows here.
#[tokio::test(start_paused = true)]
async fn interleaved_runs_store_one_reasoning_row_each_ahead_of_their_completion() {
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(1024));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let monitor = bridge.monitor();
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let task = spawn_test_bridge_until_shutdown(&bus, bridge, Duration::from_secs(3600), async {
        shutdown.await.unwrap();
    });

    let mut thoughts = [String::new(), String::new()];
    for n in 0..50 {
        for (index, run) in ["run-a", "run-b"].into_iter().enumerate() {
            let thought = format!("{run} thought {n}\n");
            thoughts[index].push_str(&thought);
            bus.emit(Event::new(MessageEvent::ReasoningDelta {
                run_id: Some(run.into()),
                delta: thought,
            }));
        }
        for run in ["run-a", "run-b"] {
            bus.emit(Event::new(MessageEvent::MessageDelta {
                run_id: Some(run.into()),
                delta: format!("{run} answer {n} "),
            }));
        }
    }
    for run in ["run-a", "run-b"] {
        bus.emit(Event::new(MessageEvent::MessageCompleted {
            run_id: run.into(),
            text: format!("{run} answer"),
        }));
    }
    // Two reasoning rows and two completions are all that reaches storage.
    monitor
        .wait_for(|state| state.persisted_events == 4 && state.pending_events == 0)
        .await;

    let rows = db.events_all_ordered().unwrap();
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert!(!rows.iter().any(|row| matches!(
        row.event.kind,
        EventKind::Message(MessageEvent::MessageDelta { .. })
    )));
    for (run, thought) in ["run-a", "run-b"].into_iter().zip(&thoughts) {
        let reasoning = reasoning_of(&rows, run);
        assert_eq!(reasoning.len(), 1, "one reasoning row for {run}");
        assert_eq!(reasoning[0].1, thought);
        assert!(
            reasoning[0].0.id < completed_id(&rows, run),
            "{run}: reasoning precedes its completion"
        );
    }
    assert_eq!(monitor.snapshot().skipped_message_deltas, 100);
    assert_eq!(monitor.snapshot().coalesced_events, 98);
    // Paused time cannot reveal that the last producer is gone; stop explicitly.
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn lag_faults_and_run_progress_are_counted_but_not_stored() {
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(1024));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let monitor = bridge.monitor();
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let task = spawn_test_bridge_until_shutdown(&bus, bridge, Duration::from_secs(3600), async {
        shutdown.await.unwrap();
    });

    for skipped in [3, 5, 8] {
        bus.emit(Event::new(FaultEvent::SubscriberLagged {
            subscriber_id: 2,
            skipped,
        }));
    }
    for _ in 0..4 {
        bus.emit(Event::new(LifecycleEvent::RunProgress {
            run_id: "run".into(),
            activity: RunActivity::Model,
            context: Some(ContextComposition::default()),
        }));
    }
    let marker = Event::new(LifecycleEvent::Started {
        session_id: "session".into(),
    });
    bus.emit(marker.clone());
    monitor.wait_for(|state| state.persisted_events == 1).await;

    // Everything before the marker was already handled, in arrival order.
    let rows = db.events_all_ordered().unwrap();
    assert_eq!(
        rows.iter().map(|row| &row.event).collect::<Vec<_>>(),
        [&marker]
    );
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.skipped_subscriber_lags, 3);
    assert_eq!(snapshot.lagged_events, 16);
    assert_eq!(snapshot.skipped_run_progress, 4);
    // Paused time cannot reveal that the last producer is gone; stop explicitly.
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn a_secret_split_across_fragments_is_rejected_once_the_row_is_merged() {
    const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut queue = EventQueue::new(
        tx,
        bridge.monitor(),
        PersistencePolicy::default(),
        storage.handle().max_event_bytes(),
    );
    let (head, tail) = SECRET.split_at(8);
    let fragment = |text: &str| {
        Event::new(MessageEvent::ReasoningDelta {
            run_id: Some("run".into()),
            delta: text.into(),
        })
    };
    // Neither fragment alone looks like a credential.
    for text in [head, tail] {
        bridge.handle_event(&fragment(text)).unwrap();
    }
    assert_eq!(db.events_all_ordered().unwrap().len(), 2);

    queue.push(fragment(head)).await.unwrap();
    queue.push(fragment(tail)).await.unwrap();
    queue.flush().await.unwrap();
    let WriteRequest::Event(merged) = rx.recv().await.unwrap() else {
        panic!("expected the merged row")
    };
    assert!(matches!(
        bridge.handle_event(&merged.event),
        Err(StorageError::SecretDetected { .. })
    ));
    assert_eq!(
        db.events_all_ordered().unwrap().len(),
        2,
        "nothing new stored"
    );
}
