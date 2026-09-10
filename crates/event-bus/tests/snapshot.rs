use event_bus::{Event, EventKind, SnapshotEvent};

#[test]
fn snapshot_roundtrip_retains_tool_and_workspace_correlation() {
    // Given: a checkpoint before a mutating tool.
    let event = Event::new(EventKind::Snapshot(SnapshotEvent {
        run_id: "run-1".into(),
        call_id: "call-2".into(),
        snapshot_id: "a".repeat(40),
        workspace_root: "/workspace".into(),
    }));
    // When: persist and reload the event.
    let encoded = serde_json::to_vec(&event).expect("encode");
    let decoded: Event = serde_json::from_slice(&encoded).expect("decode");
    // Then: correlation survives serialization.
    assert_eq!(decoded, event);
}
