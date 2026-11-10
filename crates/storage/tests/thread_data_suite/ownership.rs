use event_bus::{Event, OwnershipAction, OwnershipEvent};
use storage::{Database, Storage, StorageConfig};

#[test]
fn ownership_event_survives_ledger_roundtrip() {
    let directory = tempfile::tempdir().expect("directory");
    let config = StorageConfig {
        db_path: directory.path().join("ledger.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage");
    let event = Event::new(OwnershipEvent {
        thread_id: "thread-1".into(),
        owner_id: "owner-a".into(),
        generation: 7,
        action: OwnershipAction::Claimed,
    });
    storage.handle().append_event(None, &event).expect("append");
    let events = Database::open(&config)
        .expect("database")
        .events_all_ordered()
        .expect("read");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event);
}
