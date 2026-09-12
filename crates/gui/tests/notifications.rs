use event_bus::{AgentRunPhase, Event, LifecycleEvent, ToolEvent};
use gui::app::WorkbenchState;
use gui::model::tasks::AgentRunSource;
use workspace_ui::UiSettings;

struct EmptySource;
impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<runtime::AgentSummary> {
        Vec::new()
    }
}

#[test]
fn notifications_resolve_approval_using_post_batch_phases() {
    // Given: an empty workbench and approval preceding the waiting transition.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    let events = [
        Event::new(ToolEvent::ApprovalRequested {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Waiting,
            reason: None,
        }),
    ];
    // When: the production-shared batch fold processes the events.
    state.apply_events(events);
    // Then: the approval targets the post-batch waiting run and can be acknowledged.
    let item = state.notifications().items().next().unwrap();
    assert_eq!(item.run_id.as_deref(), Some("run-1"));
    let id = item.id;
    let revision = state.notifications().revision(id).unwrap();
    assert!(
        state
            .notifications_mut()
            .acknowledge(id, Some(&revision), Some(true))
    );
    assert_eq!(state.notifications().unread_count(), 0);
}
