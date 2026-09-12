use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event, LedgerEvent,
    LifecycleEvent,
};

#[test]
fn agent_run_restored_and_ledger_events_serde_round_trip() {
    // Given: additive lifecycle, ledger, and delivery vocabulary.
    let events = [
        Event::new(LifecycleEvent::AgentRunRestored {
            run_id: "run-1".into(),
            restored_by: "run-2".into(),
            message_id: "message-1".into(),
        }),
        Event::new(LedgerEvent::RunLedgerAppended {
            run_id: "run-1".into(),
            seq: u64::MAX,
            body: "ledger body".into(),
        }),
        Event::new(AgentMessageEvent::Delivered {
            message: AgentMessage {
                message_id: "message-1".into(),
                sender_run_id: "run-2".into(),
                recipient_run_id: "run-1".into(),
                kind: AgentMessageKind::Send,
                content: "resume".into(),
                reply_to: None,
            },
            disposition: DeliveryDisposition::Restored,
        }),
    ];
    for event in events {
        // When: a complete event crosses the JSON persistence boundary.
        let json = serde_json::to_string(&event).unwrap();
        let restored: Event = serde_json::from_str(&json).unwrap();
        // Then: every payload field and metadata value survives.
        assert_eq!(restored, event);
    }
}
