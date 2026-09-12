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

#[test]
fn duplicate_plain_call_id_across_runs_resolves_to_none() {
    // Given: two runs registered the same plain call ID.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    state.apply_events([started("call-1", "run-2"), started("call-1", "run-3")]);
    // When: an approval arrives without explicit run scope.
    state.apply_events([approval("call-1")]);
    // Then: the notification is display-only, rather than targeting the latest run.
    let item = state.notifications().items().next().unwrap();
    assert_eq!(item.run_id, None);
    assert!(state.notifications().is_unread(item.id));
}

#[test]
fn same_run_repeated_tool_started_stays_unique() {
    // Given: a run registers the same call again for a retry.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    state.apply_events([started("call-1", "run-2"), started("call-1", "run-2")]);
    // When: approval is requested for that call.
    state.apply_events([approval("call-1")]);
    // Then: repeated registration within one run remains unambiguous.
    assert_eq!(
        state
            .notifications()
            .items()
            .next()
            .unwrap()
            .run_id
            .as_deref(),
        Some("run-2")
    );
}

#[test]
fn ambiguous_fallback_stays_unresolved_while_route_keeps_latest_run() {
    use gui::model::transcript_registry::TranscriptKey;
    // Given: plain and malformed scoped IDs collide, then the latest run retries.
    for call_id in ["call-1", "run-x:call-1"] {
        let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
        state.apply_events([started(call_id, "run-2"), started(call_id, "run-3")]);
        state.apply_events([started(call_id, "run-3")]);
        // When: approval is routed and converted to a notification.
        let event = approval(call_id);
        let route = state.transcripts().route(&event);
        state.apply_events([event]);
        // Then: only notification resolution fails closed; transcript routing is unchanged.
        assert_eq!(state.notifications().items().next().unwrap().run_id, None);
        assert_eq!(
            route,
            vec![TranscriptKey::Thread, TranscriptKey::Run("run-3".into())]
        );
    }
}

#[test]
fn explicit_scope_resolves_even_when_call_index_is_ambiguous() {
    // Given: an explicitly scoped call ID has conflicting index registrations.
    let mut state = WorkbenchState::new(EmptySource, &UiSettings::default()).unwrap();
    let call_id = "run-2:call-1:0";
    state.apply_events([started(call_id, "run-3"), started(call_id, "run-4")]);
    // When: the scoped approval arrives.
    state.apply_events([approval(call_id)]);
    // Then: the explicit prefix remains authoritative.
    assert_eq!(
        state
            .notifications()
            .items()
            .next()
            .unwrap()
            .run_id
            .as_deref(),
        Some("run-2")
    );
}
