use event_bus::{Event, LedgerEvent, LifecycleEvent};
use gui::model::transcript_registry::{TranscriptKey, TranscriptRegistry};

#[test]
fn ledger_routes_to_its_run_without_transcript_content() {
    // Given: a ledger entry belonging to one run.
    let mut registry = TranscriptRegistry::new();
    let event = Event::new(LedgerEvent::RunLedgerAppended {
        run_id: "run-1".into(),
        seq: 1,
        body: "not transcript content".into(),
    });
    assert_eq!(
        registry.route(&event),
        vec![TranscriptKey::Run("run-1".into())]
    );
    // When: the registry applies the event.
    registry.apply(&event);
    // Then: the owning run exists but neither transcript gains content.
    assert!(registry.run("run-1").unwrap().entries().is_empty());
    assert!(registry.thread().entries().is_empty());
}

#[test]
fn restored_routes_like_non_error_run_lifecycle() {
    // Given: a restored run and the existing non-error run lifecycle routing.
    let registry = TranscriptRegistry::new();
    let started = Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "run-1".into(),
        parent_run_id: None,
        agent_name: "worker".into(),
        role: "worker".into(),
    });
    let restored = Event::new(LifecycleEvent::AgentRunRestored {
        run_id: "run-1".into(),
        restored_by: "run-2".into(),
        message_id: "message-1".into(),
    });
    // When: routing the restored event.
    let route = registry.route(&restored);
    // Then: restoration follows the existing lifecycle policy.
    assert_eq!(route, registry.route(&started));
}
