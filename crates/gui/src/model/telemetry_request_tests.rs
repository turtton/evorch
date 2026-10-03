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
    assert_eq!(metrics.average_ttft, Some(Duration::from_millis(200)));
    assert_eq!(metrics.average_tok_s, Some(20.0));
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
    // No reuse observation was emitted, so retention stays unknown.
    assert_eq!(metrics.cache_label(), "cache —");
}

fn reuse(run: &str, read: u64, previous: Option<u64>) -> Event {
    Event::new(ProviderEvent::CacheReuseObserved {
        request_id: format!("request-{run}"),
        cache_read_tokens: read,
        comparison: match previous {
            Some(previous_cache_tokens) => event_bus::CacheComparison::Compared {
                previous_request_id: "previous".into(),
                previous_cache_tokens,
            },
            None => event_bus::CacheComparison::NoBaseline {
                reason: event_bus::CacheBaselineMissing::NoPreviousRequest,
            },
        },
        run_id: Some(run.into()),
    })
}

fn request_started(run: &str) -> Event {
    Event::new(ProviderEvent::RequestStarted {
        request_id: format!("request-{run}"),
        provider: "provider".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "model".into(),
        streaming: true,
        run_id: Some(run.into()),
    })
}

#[test]
fn cache_label_reports_root_retention_while_billed_ratio_moves_to_tooltip() {
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&started("root", None));
    telemetry.apply_event(&started("child", Some("root")));
    // Given: the root warms up 0% -> 50% -> 75% billed, and a child breaks its cache.
    for (run, input, cached, previous) in [
        ("root", 1_000, 0, None),
        ("root", 2_000, 1_000, Some(1_000)),
        ("root", 4_000, 3_000, Some(2_000)),
        ("child", 5_000, 0, None),
        ("child", 5_000, 500, Some(5_000)),
    ] {
        telemetry.apply_event(&request_started(run));
        telemetry.apply_event(&reuse(run, cached, previous));
        for event in observed(run, input, cached, 20, 100) {
            telemetry.apply_event(&event);
        }
    }
    let thread = telemetry.thread_metrics(&["root".into(), "child".into()]);
    // Then: the cold first request does not lower the conversation retention.
    assert_eq!(thread.cache_label(), "cache 100% (avg 100%)");
    assert!(!thread.cache_reuse.latest_is_low());
    assert_eq!(
        (
            thread.cache_reuse.compared_requests,
            thread.cache_reuse.completed_requests
        ),
        (2, 3)
    );
    // The billed ratio behind the tooltip still reflects the cold request.
    assert_eq!(thread.cache_hit_rate, Some(75.0));
    assert_eq!(thread.average_cache_hit_rate.map(f64::round), Some(57.0));
    // And: the child's own metrics flag its regression.
    let child = telemetry.thread_metrics(&["child".into()]);
    assert_eq!(child.cache_reuse.average_label(), "avg cache 10.0%");
    assert!(child.cache_reuse.average_is_low());
}

#[test]
fn a_request_without_reuse_observation_does_not_keep_the_previous_value() {
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&request_started("run"));
    telemetry.apply_event(&reuse("run", 900, Some(1_000)));
    for event in observed("run", 1_000, 900, 20, 100) {
        telemetry.apply_event(&event);
    }
    // A failed attempt's observation must not leak into the next request.
    telemetry.apply_event(&request_started("run"));
    telemetry.apply_event(&reuse("run", 0, Some(1_000)));
    telemetry.apply_event(&request_started("run"));
    for event in observed("run", 1_000, 900, 20, 100) {
        telemetry.apply_event(&event);
    }
    let metrics = telemetry.thread_metrics(&["run".into()]);
    assert_eq!(metrics.cache_label(), "cache — (avg 90%)");
    assert_eq!(metrics.cache_reuse.latest, Some(RequestReuse::Unobserved));
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
    assert_eq!(pending.average_ttft, Some(Duration::from_millis(200)));
    assert_eq!(pending.average_tok_s, Some(20.0));
    assert_eq!(pending.ttft_label(), "TTFT — (avg 200ms)");
    assert_eq!(pending.tok_s_label(), "— tok/s (avg 20.0 tok/s)");
    for event in observed("run", 1_000, 500, 100, 600) {
        telemetry.apply_event_at(&event, start + Duration::from_secs(203));
    }
    let next = telemetry.thread_metrics_at(&["run".into()], start + Duration::from_secs(300));
    assert_eq!(next.ttft, Some(Duration::from_millis(600)));
    assert_eq!(next.tok_s, Some(50.0));
    assert_eq!(telemetry.row("run").unwrap().average_ttft_ms(), Some(400));
    assert_eq!(next.average_ttft, Some(Duration::from_millis(400)));
    assert_eq!(next.average_tok_s, Some(35.0));
    assert_eq!(next.ttft_label(), "TTFT 600ms (avg 400ms)");
    assert_eq!(next.tok_s_label(), "50.0 tok/s (avg 35.0 tok/s)");
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

#[test]
fn performance_averages_weight_requests_and_duration_across_root_runs() {
    let mut telemetry = TelemetryOverlay::new();
    for (run, output, ttft, duration) in [
        ("one", 80, 100, 1_000),
        ("one", 60, 500, 3_000),
        ("two", 40, 900, 2_000),
    ] {
        let [first_token, mut completed] = observed(run, 1_000, 0, output, ttft);
        if let EventKind::Provider(ProviderEvent::RequestCompleted { duration_ms, .. }) =
            &mut completed.kind
        {
            *duration_ms = duration;
        }
        telemetry.apply_event(&first_token);
        telemetry.apply_event(&completed);
    }
    let metrics = telemetry.thread_metrics(&["one".into(), "two".into()]);
    assert_eq!(metrics.ttft, Some(Duration::from_millis(900)));
    assert_eq!(metrics.average_ttft, Some(Duration::from_millis(500)));
    assert_eq!(metrics.tok_s, Some(20.0));
    assert_eq!(metrics.average_tok_s, Some(30.0));
}

#[test]
fn streaming_estimates_and_failed_requests_do_not_change_average_throughput() {
    let mut telemetry = TelemetryOverlay::new();
    for event in observed("run", 1_000, 0, 40, 200) {
        telemetry.apply_event(&event);
    }
    telemetry.apply_event(&Event::new(ProviderEvent::RequestStarted {
        request_id: "pending".into(),
        provider: "provider".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "model".into(),
        streaming: true,
        run_id: Some("run".into()),
    }));
    telemetry.apply_event(&Event::new(MessageEvent::MessageDelta {
        delta: "a".repeat(1_000),
        run_id: Some("run".into()),
    }));
    assert_eq!(
        telemetry.thread_metrics(&["run".into()]).average_tok_s,
        Some(20.0)
    );
    telemetry.apply_event(&Event::new(ProviderEvent::RequestFailed {
        request_id: "failed".into(),
        provider: "provider".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "model".into(),
        streaming: true,
        duration_ms: 10_000,
        failure: event_bus::ProviderFailureKind::Timeout,
        run_id: Some("run".into()),
    }));
    let metrics = telemetry.thread_metrics(&["run".into()]);
    assert_eq!(metrics.tok_s, None);
    assert_eq!(metrics.average_tok_s, Some(20.0));
}

#[test]
fn zero_duration_never_invents_average_throughput() {
    let mut telemetry = TelemetryOverlay::new();
    let [_, mut completed] = observed("run", 1_000, 0, 40, 200);
    if let EventKind::Provider(ProviderEvent::RequestCompleted { duration_ms, .. }) =
        &mut completed.kind
    {
        *duration_ms = 0;
    }
    telemetry.apply_event(&completed);
    let metrics = telemetry.thread_metrics(&["run".into()]);
    assert_eq!(metrics.average_ttft, None);
    assert_eq!(metrics.average_tok_s, None);
    assert_eq!(metrics.ttft_label(), "TTFT —");
    assert_eq!(metrics.tok_s_label(), "— tok/s");
}
