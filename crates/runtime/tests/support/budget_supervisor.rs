use super::*;

#[tokio::test]
async fn budget_diagnostic_blocks_once_without_terminal_continuation() {
    // Given: an active goal with a running root.
    let fixture = Fixture::new(8).await;
    let diagnostic = event_bus::DiagnosticEvent {
        source: "budget_tracker".into(),
        severity: event_bus::DiagnosticSeverity::Error,
        code: event_bus::event::diagnostic_codes::BUDGET_EXHAUSTED.into(),
        detail: "max_tool_calls=5; tool_call_count=5".into(),
        run_id: Some(fixture.root.to_string()),
        thread_id: None,
        call_id: None,
    };
    // When: exhaustion is reported twice and the run reaches its terminal phase.
    fixture.bus.emit(Event::new(diagnostic.clone()));
    fixture.bus.emit(Event::new(diagnostic));
    fixture
        .bus
        .emit(Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: fixture.root.to_string(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Error,
            reason: Some("BudgetExhausted".into()),
        }));
    fixture.settle().await;
    // Then: no fresh provider run is dispatched and only one goal transition occurs.
    assert_eq!(
        fixture
            .handle
            .snapshot(&fixture.goal_id)
            .expect("goal")
            .state,
        GoalState::Blocked
    );
    let events = fixture.orchestrator_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                OrchestratorEvent::GoalStateChanged {
                    to: GoalState::Blocked,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::ContinuationDispatched { .. }))
    );
}
