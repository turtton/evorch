use super::*;

#[test]
fn old_compaction_loads_and_new_source_round_trips_through_sqlite() {
    // Given: a stored pre-window_source payload.
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::apply_migrations(&connection).unwrap();
    let old = r#"{"kind":"Compaction","payload":{"kind":"Compacted","payload":{"run_id":"run-1","reason":"automatic","threshold":0.75,"context_window_tokens":200000,"estimated_tokens_before":150000,"estimated_tokens_after":1000,"compacted_range_start":1,"compacted_range_end":4,"checkpoint_id":"ckpt-1","summary":"summary"}}}"#;
    connection.execute("INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) VALUES ('s1', 1, 1, 1, 'compaction', ?1)", [old]).unwrap();
    // When: restoring through the real repository, then persisting the new format.
    let mut restored = list_by_session(&connection, "s1").unwrap().remove(0).event;
    let value = serde_json::to_value(&restored.kind).unwrap();
    assert_eq!(value["payload"]["payload"]["window_source"], "default");
    let mut value = value;
    value["payload"]["payload"]["window_source"] = "catalog".into();
    restored.kind = serde_json::from_value(value).unwrap();
    append_event(
        &connection,
        Some("s1"),
        &restored,
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap();
    // Then: SQLite preserves the explicit new provenance.
    let stored = list_by_session(&connection, "s1").unwrap();
    let value = serde_json::to_value(&stored[1].event.kind).unwrap();
    assert_eq!(value["payload"]["payload"]["window_source"], "catalog");
}
