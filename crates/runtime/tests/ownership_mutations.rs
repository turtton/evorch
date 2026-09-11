use event_bus::{Event, EventBus, LifecycleEvent, MessageEvent, ToolEvent};
use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
use std::sync::Arc;

#[tokio::test]
async fn delayed_run_tool_and_transcript_events_are_fenced_after_claim() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "old".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&owner).expect("start");
    let permit = OwnerPermit {
        registry_path: path,
        thread_id: owner.thread_id.clone(),
        lease: owner.lease.clone(),
        run_id: Some("run-1".into()),
    };
    let bus = EventBus::new(16);
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: directory.path().join("events.db"),
        ..storage::StorageConfig::default()
    })
    .expect("storage");
    let mut receiver = bus.subscribe();
    assert!(bus.register_mutation_fence(
        "run-1".into(),
        Arc::new(move || permit.validate_generation().is_ok())
    ));
    let events = [
        Event::new(LifecycleEvent::BackgroundTaskCompleted {
            task_id: "run-1".into(),
        }),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-1".into()),
            delta: "old".into(),
        }),
        Event::new(ToolEvent::ToolCompleted {
            run_id: Some("run-1".into()),
            tool_name: "edit".into(),
            call_id: "call".into(),
            is_error: false,
            output: None,
            detail: None,
        }),
    ];
    for event in &events {
        assert_eq!(bus.emit(event.clone()), 1);
    }
    registry
        .update("thread", |state| state.claim(&owner.lease, "next", 200, 50))
        .expect("claim");
    for event in events {
        assert_eq!(bus.emit(event), 0);
    }
    let stale = Event::new(MessageEvent::MessageDelta {
        run_id: Some("run-1".into()),
        delta: "delayed writer".into(),
    });
    assert_eq!(
        storage
            .handle()
            .append_fenced_event(None, &stale, bus.mutation_validator()),
        Err(storage::StorageError::StaleMutation)
    );
    let sentinel = Event::new(LifecycleEvent::Started {
        session_id: "unowned".into(),
    });
    bus.emit(sentinel.clone());
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .expect("bounded receive")
            .expect("event"),
        sentinel
    );
}
