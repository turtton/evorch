use super::*;

fn started(run: &str, parent: Option<&str>) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: "worker".into(),
        role: "worker".into(),
    })
}

fn observed(run: &str, input: u64, cached: u64, output: u64, ttft: u64) -> [Event; 2] {
    [
        Event::new(ProviderEvent::FirstTokenObserved {
            request_id: format!("request-{run}"),
            provider: "provider".into(),
            profile: None,
            protocol: "fixture".into(),
            model: "model".into(),
            ttft_ms: ttft,
            run_id: Some(run.into()),
        }),
        Event::new(ProviderEvent::RequestCompleted {
            request_id: format!("request-{run}"),
            provider: "provider".into(),
            profile: None,
            protocol: "fixture".into(),
            model: "model".into(),
            streaming: true,
            duration_ms: 2_000,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cached,
            cache_write_tokens: 0,
            finish_reason: "tool_use".into(),
            run_id: Some(run.into()),
        }),
    ]
}

#[test]
fn child_samples_do_not_replace_conversation_metrics_but_keep_their_cost() {
    let mut telemetry = TelemetryOverlay::new();
    for (run, parent, input, cached, output, ttft) in [
        ("root", None, 1_000, 100, 40, 200),
        ("child", Some("root"), 5_000, 5_000, 400, 900),
        ("grandchild", Some("child"), 10_000, 10_000, 800, 800),
    ] {
        telemetry.apply_event(&started(run, parent));
        for event in observed(run, input, cached, output, ttft) {
            telemetry.apply_event(&event);
        }
        telemetry.rows.get_mut(run).unwrap().context_window = Some(10_000);
        telemetry.costs.insert(run.into(), 1.0);
    }
    let metrics = telemetry.thread_metrics(&["root".into(), "child".into(), "grandchild".into()]);
    assert_eq!(metrics.cost, Some(3.0));
    assert_eq!(metrics.ttft, Some(Duration::from_millis(200)));
    assert_eq!(metrics.tok_s, Some(20.0));
    assert_eq!(metrics.context_pressure, Some(10));
    assert_eq!(metrics.cache_hit_rate, Some(10.0));
    assert_eq!(metrics.average_cache_hit_rate, Some(10.0));
    // Selecting a child conversation shows the child's own observations.
    let child = telemetry.thread_metrics(&["child".into()]);
    assert_eq!(child.ttft, Some(Duration::from_millis(900)));
    assert_eq!(child.tok_s, Some(200.0));
}

#[test]
fn cache_average_weights_all_conversation_inputs_including_cold_requests() {
    let mut telemetry = TelemetryOverlay::new();
    for (run, input, cached) in [
        ("one", 1_000, 0),
        ("one", 3_000, 3_000),
        ("two", 6_000, 6_000),
    ] {
        for event in observed(run, input, cached, 20, 100) {
            telemetry.apply_event(&event);
        }
    }
    let metrics = telemetry.thread_metrics(&["one".into(), "two".into()]);
    assert_eq!(metrics.cache_hit_rate, Some(100.0));
    assert_eq!(metrics.average_cache_hit_rate, Some(90.0));
    assert_eq!(metrics.cache_hit_rate_label(), "cache 100% (Δ90%)");
}

#[test]
fn escalation_root_retains_metrics_despite_parent_in_another_thread() {
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&started("escalation", Some("worker-elsewhere")));
    for event in observed("escalation", 1_000, 500, 40, 200) {
        telemetry.apply_event(&event);
    }
    let metrics = telemetry.thread_metrics(&["escalation".into()]);
    assert_eq!(metrics.ttft, Some(Duration::from_millis(200)));
    assert_eq!(metrics.tok_s, Some(20.0));
}

#[test]
fn tool_execution_does_not_change_provider_duration_or_current_ttft() {
    let start = Instant::now();
    let mut telemetry = TelemetryOverlay::new();
    for event in observed("run", 1_000, 500, 40, 200) {
        telemetry.apply_event_at(&event, start);
    }
    telemetry.apply_event_at(
        &Event::new(ToolEvent::ToolStarted {
            run_id: Some("run".into()),
            tool_name: "shell".into(),
            call_id: "call".into(),
            input: None,
        }),
        start + Duration::from_secs(1),
    );
    telemetry.apply_event_at(
        &Event::new(ToolEvent::ToolCompleted {
            run_id: Some("run".into()),
            tool_name: "shell".into(),
            call_id: "call".into(),
            output: None,
            is_error: false,
            detail: None,
        }),
        start + Duration::from_secs(100),
    );
    let metrics = telemetry.thread_metrics_at(&["run".into()], start + Duration::from_secs(200));
    assert_eq!(metrics.ttft, Some(Duration::from_millis(200)));
    assert_eq!(metrics.tok_s, Some(20.0));
    telemetry.apply_event_at(
        &Event::new(ProviderEvent::RequestStarted {
            request_id: "next".into(),
            provider: "provider".into(),
            profile: None,
            protocol: "fixture".into(),
            model: "model".into(),
            streaming: true,
            run_id: Some("run".into()),
        }),
        start + Duration::from_secs(201),
    );
    let pending = telemetry.thread_metrics_at(&["run".into()], start + Duration::from_secs(202));
    assert_eq!(pending.ttft, None);
    assert_eq!(pending.tok_s, None);
    for event in observed("run", 1_000, 500, 100, 600) {
        telemetry.apply_event_at(&event, start + Duration::from_secs(203));
    }
    let next = telemetry.thread_metrics_at(&["run".into()], start + Duration::from_secs(300));
    assert_eq!(next.ttft, Some(Duration::from_millis(600)));
    assert_eq!(next.tok_s, Some(50.0));
    assert_eq!(telemetry.row("run").unwrap().average_ttft_ms(), Some(400));
}

#[test]
fn continuation_is_a_conversation_root_even_when_it_has_an_owned_parent() {
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&started("root", None));
    for event in observed("root", 1_000, 500, 40, 200) {
        telemetry.apply_event(&event);
    }
    // The root-binding event may arrive before the new run's lifecycle event.
    telemetry.apply_event(&Event::new(
        event_bus::OrchestratorEvent::ContinuationDispatched {
            goal_id: "goal".into(),
            epoch: 1,
            trigger_run_id: "root".into(),
            new_run_id: "continuation".into(),
            unmet: Vec::new(),
        },
    ));
    telemetry.apply_event(&started("continuation", Some("root")));
    for event in observed("continuation", 2_000, 1_000, 100, 600) {
        telemetry.apply_event(&event);
    }
    let metrics = telemetry.thread_metrics(&["root".into(), "continuation".into()]);
    assert_eq!(metrics.ttft, Some(Duration::from_millis(600)));
    assert_eq!(metrics.tok_s, Some(50.0));
}
