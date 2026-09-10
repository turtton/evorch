#[test]
fn ownership_event_roundtrips() {
    let json = r#"{"kind":"Ownership","payload":{"thread_id":"thread-a","owner_id":"owner-a","generation":2,"action":"claimed"}}"#;
    let event: event_bus::EventKind = serde_json::from_str(json).expect("ownership event");
    assert_eq!(serde_json::to_value(event).unwrap(), serde_json::from_str::<serde_json::Value>(json).unwrap());
}
