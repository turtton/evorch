use std::sync::Arc;

use event_bus::{Event, EventBus, LifecycleEvent, WorkspaceLockHolder, WorkspaceWait};

fn wait(run: &str, waiting: bool) -> Event {
    Event::new(LifecycleEvent::WorkspaceWaitChanged {
        run_id: run.into(),
        call_id: "call-waiting".into(),
        waiting: waiting.then(|| WorkspaceWait {
            workspace_root: "/workspace".into(),
            tool_name: "shell".into(),
            command: Some("rg task".into()),
            holder: Some(WorkspaceLockHolder {
                run_id: "run-holder".into(),
                call_id: "call-owner".into(),
                tool_name: "shell".into(),
                command: Some("gh pr checks --watch".into()),
            }),
        }),
    })
}

#[test]
fn workspace_wait_round_trip_preserves_start_and_clear_correlation() {
    // Observations must retain both sides of a wait in the event ledger.
    for event in [wait("run-waiting", true), wait("run-waiting", false)] {
        let encoded = serde_json::to_vec(&event).unwrap();
        let decoded: Event = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, event);
    }
}

#[test]
fn stale_waiter_events_are_fenced_without_fencing_an_unrelated_waiter() {
    let bus = EventBus::new(8);
    let mut receiver = bus.subscribe();
    bus.register_mutation_fence("stale".into(), Arc::new(|| false));
    // A stale run cannot alter a live wait display; other runs still report theirs.
    bus.emit(wait("stale", true));
    bus.emit(wait("stale", false));
    let live = wait("live", true);
    bus.emit(live.clone());
    assert_eq!(receiver.drain_pending_snapshot(), vec![live]);
}
