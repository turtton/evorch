use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};
use gui::{model::transcript_registry::TranscriptRegistry, storage_bridge::StorageBridge};
use storage::{Database, Storage, StorageConfig};

#[test]
fn diagnostic_survives_storage_replay_and_is_visible_in_owning_thread() {
    // Given: a real ledger, and diagnostics with explicit thread attribution.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = StorageConfig {
        db_path: dir.path().join("ledger.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage");
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let events: Vec<_> = [
        DiagnosticSeverity::Info,
        DiagnosticSeverity::Warning,
        DiagnosticSeverity::Error,
    ]
    .into_iter()
    .map(|severity| {
        Event::new(DiagnosticEvent {
            source: "process_owner".into(),
            severity,
            code: "claim_conflict".into(),
            detail: format!("{} diagnostic detail", severity.as_str()),
            run_id: Some("run-1".into()),
            thread_id: Some("thread-1".into()),
        })
    })
    .collect();
    // When: persisting, reopening, and replaying while a different thread is active.
    for event in &events {
        bridge.handle_event(event).expect("append");
    }
    drop(bridge);
    drop(storage);
    let db = Database::open(&config).expect("reopen");
    let stored = db.events_all_ordered().expect("ordered events");
    let mut registry = TranscriptRegistry::new();
    registry.select_thread(Some("other-thread".into()));
    for event in &stored {
        registry.apply(&event.event);
    }
    // Then: exact ordered payloads survive, with no session state mutation or cross-thread leak.
    assert_eq!(
        stored
            .iter()
            .map(|value| value.event.clone())
            .collect::<Vec<_>>(),
        events
    );
    assert_eq!(
        db.events_by_session("session").expect("session events"),
        stored
    );
    assert!(db.restore_sessions().expect("projection").is_empty());
    assert!(registry.thread().entries().is_empty());
    registry.select_thread(Some("thread-1".into()));
    assert_eq!(registry.thread().entries().len(), 3);
    assert_eq!(
        registry.run("run-1").expect("run").entries(),
        registry.thread().entries()
    );
    let mut harness = Harness::new_ui_state(
        |ui, registry| {
            gui::theme::install(ui.ctx());
            gui::panes::agent_transcript::agent_transcript_pane(
                ui,
                "thread-1",
                Some(registry.thread()),
            );
        },
        registry,
    );
    harness.run_steps(2);
    for severity in ["info", "warning", "error"] {
        assert!(
            harness
                .query_by_label_contains(&format!("{severity} diagnostic detail"))
                .is_some()
        );
    }
    assert!(
        harness
            .query_by_label_contains("[info] process_owner (claim_conflict)")
            .is_some()
    );
}
