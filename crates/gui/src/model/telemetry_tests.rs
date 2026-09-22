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

fn agent_run_changed(run_id: &str, from: AgentRunPhase, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from,
        to,
        reason: None,
    })
}

#[test]
fn wall_time_accumulates_between_running_and_terminal_state() {
    // Given: a run that spends 10 seconds Pending before Running.
    let mut overlay = TelemetryOverlay::new();
    let start = Instant::now();
    overlay.apply_event_at(&agent_run_started("run-1"), start);
    overlay.apply_event_at(
        &agent_run_changed("run-1", AgentRunPhase::Pending, AgentRunPhase::Running),
        start + Duration::from_secs(10),
    );
    overlay.apply_event_at(
        &agent_run_changed("run-1", AgentRunPhase::Running, AgentRunPhase::Done),
        start + Duration::from_secs(90),
    );
    // When: reading metrics after completion.
    let metrics =
        overlay.thread_metrics_at(&["run-1".to_owned()], start + Duration::from_secs(120));
    // Then: only the 80 Running seconds remain.
    assert_eq!(metrics.wall_time, Duration::from_secs(80));
}

#[test]
fn wall_time_includes_in_flight_runs() {
    // Given: one failed run and another still Running, both initially Pending.
    let mut overlay = TelemetryOverlay::new();
    let start = Instant::now();
    overlay.apply_event_at(&agent_run_started("run-1"), start);
    overlay.apply_event_at(
        &agent_run_changed("run-1", AgentRunPhase::Pending, AgentRunPhase::Running),
        start + Duration::from_secs(10),
    );
    overlay.apply_event_at(
        &agent_run_changed("run-1", AgentRunPhase::Running, AgentRunPhase::Error),
        start + Duration::from_secs(30),
    );
    overlay.apply_event_at(&agent_run_started("run-2"), start + Duration::from_secs(40));
    overlay.apply_event_at(
        &agent_run_changed("run-2", AgentRunPhase::Pending, AgentRunPhase::Running),
        start + Duration::from_secs(50),
    );
    // When: reading both runs at 100 seconds.
    let metrics = overlay.thread_metrics_at(
        &["run-1".to_owned(), "run-2".to_owned()],
        start + Duration::from_secs(100),
    );
    // Then: the completed 20 seconds and in-flight 50 seconds are summed.
    assert_eq!(metrics.wall_time, Duration::from_secs(70));
}

fn wall_time_after_phases(phases: &[(AgentRunPhase, u64)]) -> Duration {
    let mut overlay = TelemetryOverlay::new();
    let start = Instant::now();
    overlay.apply_event_at(&agent_run_started("run-1"), start);
    let mut from = AgentRunPhase::Pending;
    for &(to, seconds) in phases {
        overlay.apply_event_at(
            &agent_run_changed("run-1", from, to),
            start + Duration::from_secs(seconds),
        );
        from = to;
    }
    overlay
        .thread_metrics_at(&["run-1".to_owned()], start + Duration::from_secs(120))
        .wall_time
}

#[test]
fn wall_time_is_zero_when_waiting_until_done() {
    // Given: an immediate Running -> Waiting transition, then Done much later.
    use AgentRunPhase::{Done, Running, Waiting};
    // When: replaying the lifecycle with no elapsed Running interval.
    let elapsed = wall_time_after_phases(&[(Running, 0), (Waiting, 0), (Done, 90)]);
    // Then: user Waiting contributes nothing.
    assert_eq!(elapsed, Duration::ZERO);
}

#[test]
fn wall_time_accumulates_running_intervals_across_waiting() {
    // Given: Running intervals [10, 30] and [80, 90].
    use AgentRunPhase::{Done, Running, Waiting};
    // When: the run resumes and completes.
    let elapsed =
        wall_time_after_phases(&[(Running, 10), (Waiting, 30), (Running, 80), (Done, 90)]);
    // Then: only those two intervals count.
    assert_eq!(elapsed, Duration::from_secs(30));
}

#[test]
fn wall_time_is_frozen_while_waiting() {
    // Given: a run paused after 20 Running seconds.
    use AgentRunPhase::{Running, Waiting};
    // When: reading at 120 seconds without resuming.
    let elapsed = wall_time_after_phases(&[(Running, 10), (Waiting, 30)]);
    // Then: the waiting interval is excluded even before completion.
    assert_eq!(elapsed, Duration::from_secs(20));
}

#[test]
fn wall_time_is_zero_when_run_stays_pending() {
    // Given: a registered run with no phase transitions.
    // When: reading at 120 seconds.
    let elapsed = wall_time_after_phases(&[]);
    // Then: registration alone does not start the clock.
    assert_eq!(elapsed, Duration::ZERO);
}

#[test]
fn wall_time_preserves_active_start_on_duplicate_running_events() {
    // Given: a repeated Running notification before an error.
    use AgentRunPhase::{Error, Running};
    // When: replaying duplicate Running and terminal notifications.
    let elapsed = wall_time_after_phases(&[(Running, 10), (Running, 20), (Error, 30), (Error, 40)]);
    // Then: neither the start nor the completed duration is counted twice.
    assert_eq!(elapsed, Duration::from_secs(20));
}

#[test]
fn thread_metrics_uses_latest_cache_hit_rate_across_runs() {
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&request_completed(Some("run-1"), 1000, 10));
    overlay.apply_event(&request_completed(Some("run-2"), 100, 10));
    let metrics = overlay.thread_metrics(&["run-2".to_owned(), "run-1".to_owned()]);
    let rate = metrics.cache_hit_rate.expect("cache hit rate");
    assert_eq!(rate, 3.0 / 100.0 * 100.0);
    assert!(metrics.cost.is_none());
}

#[test]
fn thread_metrics_empty_for_unknown_runs() {
    let overlay = TelemetryOverlay::new();
    let metrics = overlay.thread_metrics(&["missing".to_owned()]);
    assert_eq!(metrics, ThreadMetrics::default());
}

#[test]
fn thread_metrics_cache_rate_is_zero_when_total_input_is_zero() {
    // Given: completed usage with cache writes but zero input.
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&request_completed(Some("run-1"), 0, 10));
    // When: aggregating the thread's completed usage.
    let metrics = overlay.thread_metrics(&["run-1".to_owned()]);
    // Then: inconsistent cache subtotals do not invent a total input count.
    assert_eq!(metrics.cache_hit_rate, Some(0.0));
}

#[test]
fn thread_metrics_cache_rate_excludes_cold_start_in_same_run() {
    // Given: cold then warm responses within one run.
    let mut overlay = TelemetryOverlay::new();
    for (input, reads, writes) in [(20_000, 0, 20_000), (20_500, 20_000, 0)] {
        let mut event = request_completed(Some("run-1"), input, 10);
        if let EventKind::Provider(ProviderEvent::RequestCompleted {
            cache_read_tokens,
            cache_write_tokens,
            ..
        }) = &mut event.kind
        {
            *cache_read_tokens = reads;
            *cache_write_tokens = writes;
        }
        overlay.apply_event(&event);
    }
    // When: reading the header metric.
    let rate = overlay.thread_metrics(&["run-1".into()]).cache_hit_rate;
    // Then: only the warm response determines the rate.
    assert_eq!(rate, Some(20_000.0 / 20_500.0 * 100.0));
}
