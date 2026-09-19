use event_bus::CompactionEvent;

const OLD: &str = r#"{"kind":"Compacted","payload":{"run_id":"run-1","reason":"automatic","threshold":0.75,"context_window_tokens":200000,"estimated_tokens_before":150000,"estimated_tokens_after":1000,"compacted_range_start":1,"compacted_range_end":4,"checkpoint_id":"ckpt-1","summary":"summary"}}"#;

#[test]
fn window_source_round_trips_when_present() {
    for source in ["override", "catalog", "default"] {
        // Given: a new-format compaction payload.
        let mut value: serde_json::Value = serde_json::from_str(OLD).unwrap();
        value["payload"]["window_source"] = source.into();
        // When: passing through the typed serde boundary.
        let event: CompactionEvent = serde_json::from_value(value.clone()).unwrap();
        let serialized = serde_json::to_value(event).unwrap();
        // Then: provenance survives.
        assert_eq!(serialized, value);
    }
}

#[test]
fn window_source_defaults_when_old_payload_is_loaded() {
    // Given: a literal event written before window_source existed.
    // When: deserializing it and serializing the typed event.
    let event: CompactionEvent = serde_json::from_str(OLD).unwrap();
    let serialized = serde_json::to_value(event).unwrap();
    // Then: legacy events load with the backward-compatible default.
    assert_eq!(serialized["payload"]["window_source"], "default");
}
