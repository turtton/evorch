use super::*;

fn assert_no_event(receiver: &mut event_bus::EventReceiver) {
    let mut receive = std::pin::pin!(receiver.recv());
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(receive.as_mut(), &mut context).is_pending());
}

#[tokio::test]
async fn token_warning_is_latched_without_stopping_execution() {
    // Given: token usage alone reaches the remaining-budget threshold.
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    let settings = BudgetSettings {
        max_tokens: Some(100),
        ..Default::default()
    };
    let context = BudgetContext {
        bus: &bus,
        run_id: "run",
        task_id: "task",
        settings: &settings,
    };
    let mut counters = BudgetCounters::default();
    counters.usage(Usage {
        input_tokens: 79,
        ..Default::default()
    });
    assert_eq!(counters.publish(0, &context), BudgetDecision::Continue);
    assert_no_event(&mut receiver);
    counters.usage(Usage {
        output_tokens: 1,
        ..Default::default()
    });
    // When: repeated boundaries observe the warning threshold.
    for _ in 0..3 {
        assert_eq!(counters.publish(0, &context), BudgetDecision::Continue);
    }
    // Then: one warning reports both remaining allowances.
    let event = receiver.recv().await.expect("warning");
    assert!(matches!(event.kind, event_bus::EventKind::Diagnostic(d)
        if d.code == diagnostic_codes::BUDGET_WARNING
            && d.severity == DiagnosticSeverity::Warning
            && d.detail.contains("remaining_tool_calls=400")
            && d.detail.contains("remaining_tokens=20")));
    assert_no_event(&mut receiver);
}

#[tokio::test]
async fn warning_is_suppressed_when_exhaustion_is_already_observed() {
    // Given: the first boundary has already exhausted token capacity.
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    let settings = BudgetSettings {
        max_tokens: Some(100),
        ..Default::default()
    };
    let context = BudgetContext {
        bus: &bus,
        run_id: "run",
        task_id: "task",
        settings: &settings,
    };
    let mut counters = BudgetCounters::default();
    counters.usage(Usage {
        input_tokens: 101,
        ..Default::default()
    });
    // When: multiple boundaries publish the exhausted state.
    for _ in 0..3 {
        assert!(matches!(
            counters.publish(0, &context),
            BudgetDecision::Exhausted(_)
        ));
    }
    // Then: only the hard error is emitted, never a late warning.
    let event = receiver.recv().await.expect("exhaustion");
    assert!(matches!(event.kind, event_bus::EventKind::Diagnostic(d)
        if d.code == diagnostic_codes::BUDGET_EXHAUSTED && d.severity == DiagnosticSeverity::Error));
    assert_no_event(&mut receiver);
}

#[test]
fn successful_tool_progress_resets_consecutive_no_progress_rounds() {
    // Given: two rounds without successful tool activity.
    let mut counters = BudgetCounters::default();
    counters.finish_round();
    counters.finish_round();
    assert_eq!(counters.no_progress_rounds, 2);
    // When: a round records successful tool progress.
    counters.mark_progress();
    counters.finish_round();
    // Then: the consecutive counter resets.
    assert_eq!(counters.no_progress_rounds, 0);
}

#[test]
fn no_activity_after_progress_increments_again() {
    // Given: a previous round made progress.
    let mut counters = BudgetCounters::default();
    counters.mark_progress();
    counters.finish_round();
    // When: the next round has no successful activity.
    counters.finish_round();
    // Then: progress is not carried into subsequent rounds.
    assert_eq!(counters.no_progress_rounds, 1);
}

#[test]
fn runtime_default_allows_one_hundred_no_progress_rounds() {
    // Given/When: runtime defaults are constructed independently of config.
    let settings = BudgetSettings::default();
    // Then: the runtime uses the same hundred-round allowance.
    assert_eq!(settings.max_no_progress_rounds, 100);
}

#[test]
fn usage_accumulation_saturates_without_wrapping() {
    // Given: counters close to their storage limits.
    let mut counters = BudgetCounters::default();
    counters.usage(Usage {
        input_tokens: u64::MAX,
        output_tokens: u64::MAX,
        ..Default::default()
    });
    // When: more usage arrives.
    counters.usage(Usage {
        input_tokens: 1,
        output_tokens: 2,
        ..Default::default()
    });
    // Then: both counters saturate instead of resetting the budget.
    assert_eq!(counters.cumulative_input_tokens, u64::MAX);
    assert_eq!(counters.cumulative_output_tokens, u64::MAX);
}

#[test]
fn cumulative_budget_includes_cached_input_exactly_once() {
    let mut counters = BudgetCounters::default();
    for _ in 0..2 {
        counters.usage(Usage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 60,
            cache_write_tokens: 30,
        });
    }
    assert_eq!(counters.cumulative_input_tokens, 200);
    assert_eq!(counters.cumulative_output_tokens, 20);
}

#[tokio::test]
async fn default_run_continues_past_two_million_cumulative_tokens() {
    let bus = EventBus::new(8);
    let mut receiver = bus.subscribe();
    let settings = BudgetSettings::default();
    let context = BudgetContext {
        bus: &bus,
        run_id: "run",
        task_id: "run-62",
        settings: &settings,
    };
    let mut counters = BudgetCounters::default();
    for _ in 0..57 {
        counters.usage(Usage {
            input_tokens: 36_000,
            output_tokens: 500,
            cache_read_tokens: 34_000,
            ..Default::default()
        });
    }
    assert!(counters.cumulative_input_tokens + counters.cumulative_output_tokens > 2_000_000);
    assert_eq!(counters.publish(65, &context), BudgetDecision::Continue);
    assert_no_event(&mut receiver);
}
