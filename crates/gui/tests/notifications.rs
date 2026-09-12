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
fn notifications_resolve_approval_using_call_index() {
    // Given: a tool call attributed to a run without a Waiting transition.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    let events = [
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
            run_id: Some("run-1".into()),
            input: None,
        }),
        Event::new(ToolEvent::ApprovalRequested {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
        }),
    ];
    // When: the production-shared batch fold processes the events.
    state.apply_events(events);
    // Then: the approval targets the indexed run and can be acknowledged.
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

fn started(call_id: &str, run_id: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: call_id.into(),
        run_id: Some(run_id.into()),
        input: None,
    })
}

fn approval(call_id: &str) -> Event {
    Event::new(ToolEvent::ApprovalRequested {
        tool_name: "shell".into(),
        call_id: call_id.into(),
    })
}

fn waiting(run_id: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Waiting,
        reason: None,
    })
}

#[test]
fn approval_targets_own_run_when_unrelated_run_is_waiting() {
    // Given: an idle chat and a background tool call.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    state.apply_events([waiting("idle-chat"), started("call-b", "background-b")]);
    // When: the background run requests approval.
    state.apply_events([approval("call-b")]);
    // Then: the idle chat cannot steal its target.
    let item = state.notifications().items().next().unwrap();
    assert_eq!(item.run_id.as_deref(), Some("background-b"));
}

#[test]
fn approval_has_no_target_when_call_id_is_unknown() {
    // Given: an idle chat and an unrelated indexed call.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    state.apply_events([waiting("idle-chat"), started("known", "background-b")]);
    // When: an unknown call requests approval.
    state.apply_events([approval("unknown")]);
    // Then: the notification remains display-only and unread.
    let item = state.notifications().items().next().unwrap();
    assert_eq!(item.run_id, None);
    assert!(state.notifications().is_unread(item.id));
}

#[test]
fn approval_resolves_call_started_in_earlier_batch() {
    // Given: a call indexed by an earlier batch, with no Waiting run.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    state.apply_events([started("call-b", "background-b")]);
    // When: a later batch requests approval.
    state.apply_events([approval("call-b")]);
    // Then: the registry retains the target across batches.
    let item = state.notifications().items().next().unwrap();
    assert_eq!(item.run_id.as_deref(), Some("background-b"));
}

#[test]
fn concurrent_approvals_resolve_each_calls_own_run() {
    // Given: calls from two concurrent runs.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    state.apply_events([started("call-a", "run-a"), started("call-b", "run-b")]);
    // When: approvals arrive in reverse order in one batch.
    state.apply_events([approval("call-b"), approval("call-a")]);
    // Then: each notification retains its own call's target.
    let targets: Vec<_> = state
        .notifications()
        .items()
        .map(|item| item.run_id.as_deref())
        .collect();
    assert_eq!(targets, [Some("run-b"), Some("run-a")]);
}
