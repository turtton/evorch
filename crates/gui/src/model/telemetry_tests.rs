use super::*;

fn request_started(run_id: Option<&str>) -> Event {
    Event::new(ProviderEvent::RequestStarted {
        request_id: "request-1".into(),
        provider: "provider-a".into(),
        profile: None,
        protocol: "protocol-a".into(),
        model: "model-a".into(),
        streaming: true,
        run_id: run_id.map(str::to_owned),
    })
}

fn request_completed(run_id: Option<&str>, input: u64, output: u64) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: "request-1".into(),
        provider: "provider-a".into(),
        profile: None,
        protocol: "protocol-a".into(),
        model: "model-a".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
        finish_reason: "stop".into(),
        run_id: run_id.map(str::to_owned),
    })
}

#[test]
fn provider_and_model_come_from_request_started() {
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&request_started(Some("run-1")));
    let row = overlay.row("run-1").expect("telemetry row");
    assert_eq!(row.provider.as_deref(), Some("provider-a"));
    assert_eq!(row.model.as_deref(), Some("model-a"));
    assert_eq!(row.requests, 1);
}

#[test]
fn tokens_accumulate_from_request_completed_only() {
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&request_completed(Some("run-1"), 10, 20));
    overlay.apply_event(&request_completed(Some("run-1"), 5, 7));
    assert_eq!(
        overlay.row("run-1").expect("telemetry row").usage,
        TokenUsage {
            input: 15,
            output: 27,
            cache_read: 6,
            cache_write: 8
        }
    );
}

#[test]
fn current_tool_set_and_cleared() {
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&Event::new(ToolEvent::ToolStarted {
        input: None,
        tool_name: "read".into(),
        call_id: "call-1".into(),
        run_id: Some("run-1".into()),
    }));
    assert_eq!(
        overlay
            .row("run-1")
            .expect("telemetry row")
            .current_tool
            .as_deref(),
        Some("read")
    );
    overlay.apply_event(&Event::new(ToolEvent::ToolCompleted {
        output: None,
        tool_name: "read".into(),
        call_id: "call-1".into(),
        is_error: false,
        detail: None,
        run_id: Some("run-1".into()),
    }));
    assert!(
        overlay
            .row("run-1")
            .expect("telemetry row")
            .current_tool
            .is_none()
    );
}

fn agent_run_started(run_id: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run_id.into(),
        parent_run_id: None,
        agent_name: "agent".into(),
        role: "worker".into(),
    })
}

fn agent_run_finished(run_id: &str, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from: AgentRunPhase::Running,
        to,
        reason: None,
    })
}

#[test]
fn wall_time_accumulates_between_run_start_and_terminal_state() {
    let mut overlay = TelemetryOverlay::new();
    let start = Instant::now();
    overlay.apply_event_at(&agent_run_started("run-1"), start);
    overlay.apply_event_at(
        &agent_run_finished("run-1", AgentRunPhase::Done),
        start + Duration::from_secs(90),
    );
    let metrics =
        overlay.thread_metrics_at(&["run-1".to_owned()], start + Duration::from_secs(120));
    assert_eq!(metrics.wall_time, Duration::from_secs(90));
}

#[test]
fn wall_time_includes_in_flight_runs() {
    let mut overlay = TelemetryOverlay::new();
    let start = Instant::now();
    overlay.apply_event_at(&agent_run_started("run-1"), start);
    overlay.apply_event_at(
        &agent_run_finished("run-1", AgentRunPhase::Error),
        start + Duration::from_secs(30),
    );
    overlay.apply_event_at(&agent_run_started("run-2"), start + Duration::from_secs(40));
    let metrics = overlay.thread_metrics_at(
        &["run-1".to_owned(), "run-2".to_owned()],
        start + Duration::from_secs(100),
    );
    assert_eq!(metrics.wall_time, Duration::from_secs(90));
}

#[test]
fn thread_metrics_aggregates_cache_hit_rate_across_runs() {
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&request_completed(Some("run-1"), 100, 10));
    overlay.apply_event(&request_completed(Some("run-2"), 100, 10));
    let metrics = overlay.thread_metrics(&["run-1".to_owned(), "run-2".to_owned()]);
    let rate = metrics.cache_hit_rate.expect("cache hit rate");
    assert_eq!(rate, 3.0);
    assert!(metrics.cost.is_none());
}

#[test]
fn thread_metrics_empty_for_unknown_runs() {
    let overlay = TelemetryOverlay::new();
    let metrics = overlay.thread_metrics(&["missing".to_owned()]);
    assert_eq!(metrics, ThreadMetrics::default());
}

#[test]
fn thread_metrics_hides_cache_rate_when_input_is_zero() {
    // Given: cache counters without an input denominator.
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&request_completed(Some("run-1"), 0, 10));
    // When: aggregating the thread's completed usage.
    let metrics = overlay.thread_metrics(&["run-1".to_owned()]);
    // Then: an absent denominator is not displayed as a rate.
    assert_eq!(metrics.cache_hit_rate, None);
}
