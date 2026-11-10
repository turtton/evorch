use super::*;

fn assert_no_event(receiver: &mut event_bus::EventReceiver) {
    let mut receive = std::pin::pin!(receiver.recv());
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(receive.as_mut(), &mut context).is_pending());
}

#[tokio::test(start_paused = true)]
async fn elapsed_budget_excludes_waiting_and_resumes_active_accounting() {
    // Given: 上限より短い実行時間の後に待機する。
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    let settings = BudgetSettings {
        max_elapsed: ResourceLimit::Bounded(Duration::from_millis(250)),
        ..Default::default()
    };
    let context = BudgetContext {
        bus: &bus,
        run_id: "run",
        task_id: "task",
        settings: &settings,
    };
    let mut counters = BudgetCounters::default();
    tokio::time::advance(Duration::from_millis(100)).await;
    counters.pause();

    // When: 待機中に上限を超えてから再開する。
    tokio::time::advance(Duration::from_secs(2)).await;
    counters.resume();

    // Then: 待機時間は除外され、再開後の実行時間だけが超過を引き起こす。
    assert_eq!(counters.active_elapsed(), Duration::from_millis(100));
    assert_eq!(counters.publish(0, &context), BudgetDecision::Continue);
    assert_no_event(&mut receiver);
    tokio::time::advance(Duration::from_millis(151)).await;
    assert!(
        matches!(counters.publish(0, &context), BudgetDecision::Exhausted(breach)
        if breach.code == diagnostic_codes::BUDGET_EXHAUSTED
            && breach.detail.contains("max_elapsed_ms=250"))
    );
    let event = receiver.recv().await.expect("exhaustion");
    assert!(matches!(event.kind, event_bus::EventKind::Diagnostic(d)
        if d.source == "budget_tracker"
            && d.code == diagnostic_codes::BUDGET_EXHAUSTED));
    assert_no_event(&mut receiver);
}

#[tokio::test(start_paused = true)]
async fn publishing_while_paused_excludes_current_and_previous_waits() {
    // Given: 実行時間と確定済みの待機時間がある。
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    let settings = BudgetSettings {
        max_elapsed: ResourceLimit::Bounded(Duration::from_millis(250)),
        ..Default::default()
    };
    let context = BudgetContext {
        bus: &bus,
        run_id: "run",
        task_id: "task",
        settings: &settings,
    };
    let mut counters = BudgetCounters::default();
    tokio::time::advance(Duration::from_millis(100)).await;
    counters.pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    counters.resume();
    tokio::time::advance(Duration::from_millis(50)).await;
    counters.pause();

    // When: 次の待機中に publish する。
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(counters.publish(50, &context), BudgetDecision::Continue);

    // Then: 進行中の待機も除外され、checkpoint も実行時間のみを報告する。
    let event = receiver.recv().await.expect("checkpoint");
    assert!(matches!(
        event.kind,
        event_bus::EventKind::Orchestrator(OrchestratorEvent::TaskCheckpoint {
            elapsed_ms: 150,
            ..
        })
    ));
    assert_no_event(&mut receiver);
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(counters.publish(50, &context), BudgetDecision::Continue);
    assert_eq!(counters.active_elapsed(), Duration::from_millis(150));
    assert_no_event(&mut receiver);
}

#[tokio::test(start_paused = true)]
async fn pause_is_idempotent_and_resume_without_pause_is_a_no_op() {
    // Given: pause していない状態の resume は実行時間を変えない。
    let mut counters = BudgetCounters::default();
    tokio::time::advance(Duration::from_millis(100)).await;
    counters.resume();
    assert_eq!(counters.active_elapsed(), Duration::from_millis(100));
    counters.pause();

    // When: 待機中に重ねて pause してから再開する。
    tokio::time::advance(Duration::from_secs(1)).await;
    counters.pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(counters.active_elapsed(), Duration::from_millis(100));
    counters.resume();

    // Then: 最初の pause からの全時間が除外され、余分な resume は無害。
    assert_eq!(counters.paused_total, Duration::from_secs(2));
    assert_eq!(counters.active_elapsed(), Duration::from_millis(100));
    tokio::time::advance(Duration::from_millis(50)).await;
    counters.resume();
    assert_eq!(counters.paused_total, Duration::from_secs(2));
    assert_eq!(counters.active_elapsed(), Duration::from_millis(150));
}

#[tokio::test]
async fn token_warning_is_latched_without_stopping_execution() {
    // Given: token usage alone reaches the remaining-budget threshold.
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    let settings = BudgetSettings {
        max_tokens: ResourceLimit::Bounded(100),
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
        max_tokens: ResourceLimit::Bounded(100),
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
fn child_spawn_counts_as_progress_every_time() {
    // Given: rounds that only start child runs.
    let mut counters = BudgetCounters::default();
    counters.finish_round();
    for _ in 0..3 {
        // When: each round successfully spawns a child.
        counters.mark_child_spawned();
        counters.finish_round();
        // Then: every spawn resets the consecutive no-progress counter.
        assert_eq!(counters.no_progress_rounds, 0);
    }
}

#[test]
fn child_completion_counts_once_per_child() {
    // Given: a child whose terminal result is observed for the first time.
    let mut counters = BudgetCounters::default();
    counters.finish_round();
    assert!(counters.mark_child_completed("run-child-a"));
    counters.finish_round();
    assert_eq!(counters.no_progress_rounds, 0);
    // When: the same child result is observed again in later rounds.
    assert!(!counters.mark_child_completed("run-child-a"));
    counters.finish_round();
    assert!(!counters.mark_child_completed("run-child-a"));
    counters.finish_round();
    // Then: repeated observation is not progress.
    assert_eq!(counters.no_progress_rounds, 2);
    // When: a different child completes.
    assert!(counters.mark_child_completed("run-child-b"));
    counters.finish_round();
    // Then: the new completion counts as progress.
    assert_eq!(counters.no_progress_rounds, 0);
}

#[test]
fn repeated_child_completion_does_not_hide_other_progress() {
    // Given: a child completion that was already counted.
    let mut counters = BudgetCounters::default();
    assert!(counters.mark_child_completed("run-child"));
    counters.finish_round();
    // When: a round re-observes it but also makes unrelated progress.
    counters.mark_progress();
    assert!(!counters.mark_child_completed("run-child"));
    counters.finish_round();
    // Then: the round still counts as progress.
    assert_eq!(counters.no_progress_rounds, 0);
}

#[test]
fn spawn_credits_run_out_without_an_observed_completion() {
    // Given: a parent that only spawns children and never collects a result.
    let mut counters = BudgetCounters::default();
    for _ in 0..super::MAX_UNOBSERVED_SPAWN_CREDITS {
        // When: spawns stay within the credit limit.
        assert!(counters.mark_child_spawned());
        counters.finish_round();
        // Then: each still counts as progress.
        assert_eq!(counters.no_progress_rounds, 0);
    }
    // When: further spawns exceed the limit.
    for expected in 1..=3 {
        assert!(!counters.mark_child_spawned());
        counters.finish_round();
        // Then: they no longer reset the no-progress counter.
        assert_eq!(counters.no_progress_rounds, expected);
    }
}

#[test]
fn newly_observed_completion_restores_spawn_credits() {
    // Given: exhausted spawn credits.
    let mut counters = BudgetCounters::default();
    for _ in 0..=super::MAX_UNOBSERVED_SPAWN_CREDITS {
        counters.mark_child_spawned();
    }
    assert!(!counters.mark_child_spawned());
    // When: a child completion is observed for the first time.
    assert!(counters.mark_child_completed("run-child-a"));
    // Then: the full credit limit is available again.
    for _ in 0..super::MAX_UNOBSERVED_SPAWN_CREDITS {
        assert!(counters.mark_child_spawned());
    }
    assert!(!counters.mark_child_spawned());
    // When/Then: re-observing the same child does not restore credits ...
    assert!(!counters.mark_child_completed("run-child-a"));
    assert!(!counters.mark_child_spawned());
    // ... but a different, newly observed child does.
    assert!(counters.mark_child_completed("run-child-b"));
    assert!(counters.mark_child_spawned());
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
            context_tokens: None,
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 60,
            cache_write_tokens: 30,
            reasoning_tokens: None,
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

fn context<'a>(bus: &'a EventBus, settings: &'a BudgetSettings) -> BudgetContext<'a> {
    BudgetContext {
        bus,
        run_id: "run",
        task_id: "task",
        settings,
    }
}

#[test]
fn lane_scope_lifts_only_quantity_caps() {
    // Given: the default settings and the lane-scope settings derived from them.
    let default = BudgetSettings::default();
    let lane = BudgetSettings::lane_scope();
    // Then: time, tool-call and token caps are explicitly unlimited ...
    assert_eq!(lane.max_tool_calls, ResourceLimit::Unlimited);
    assert_eq!(lane.max_elapsed, ResourceLimit::Unlimited);
    assert_eq!(lane.max_tokens, ResourceLimit::Unlimited);
    // ... the runaway diagnostics are untouched ...
    assert_eq!(lane.max_no_progress_rounds, default.max_no_progress_rounds);
    assert_eq!(lane.max_file_rereads, default.max_file_rereads);
    assert_eq!(
        lane.max_identical_tool_call_repeats,
        default.max_identical_tool_call_repeats
    );
    // ... and it cannot be mistaken for the "unspecified" default.
    assert_ne!(lane, default);
    // The same lifting applies to explicit non-default settings, keeping their diagnostics.
    let custom = BudgetSettings {
        max_no_progress_rounds: 3,
        max_identical_tool_call_repeats: 2,
        ..Default::default()
    }
    .with_unlimited_resources();
    assert_eq!(custom.max_no_progress_rounds, 3);
    assert_eq!(custom.max_identical_tool_call_repeats, 2);
    assert_eq!(custom.max_tool_calls, ResourceLimit::Unlimited);
}

#[test]
fn resource_limit_distinguishes_unlimited_from_any_bound() {
    // Unlimited never trips, even for the largest usage.
    assert!(!ResourceLimit::<u32>::Unlimited.is_reached_by(u32::MAX));
    assert!(!ResourceLimit::<u64>::Unlimited.is_exceeded_by(u64::MAX));
    assert_eq!(ResourceLimit::<u32>::Unlimited.remaining(7), None);
    assert_eq!(ResourceLimit::<u32>::Unlimited.bounded(), None);
    // A bound of zero is a real cap, not "no cap".
    assert!(ResourceLimit::Bounded(0_u32).is_reached_by(0));
    assert!(!ResourceLimit::Bounded(0_u32).is_exceeded_by(0));
    assert!(ResourceLimit::Bounded(0_u32).is_exceeded_by(1));
    assert_eq!(ResourceLimit::Bounded(10_u32).remaining(4), Some(6));
    assert_eq!(ResourceLimit::Bounded(10_u64).remaining(40), Some(0));
}

#[tokio::test(start_paused = true)]
async fn unlimited_resources_never_stop_on_volume_alone() {
    // Given: lane-scope settings, whose default counterpart would long since have stopped.
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    let settings = BudgetSettings::lane_scope();
    let context = context(&bus, &settings);
    let mut counters = BudgetCounters::default();
    // When: virtual time, tokens and tool calls far exceed every default cap.
    tokio::time::advance(Duration::from_secs(10 * 60 * 60)).await;
    counters.usage(Usage {
        input_tokens: u64::MAX / 2,
        output_tokens: u64::MAX / 2,
        ..Default::default()
    });
    // Then: no exhaustion and no warning, while usage is still metered.
    for tool_calls in [399, 401, 10_001, u32::MAX] {
        assert_eq!(
            counters.publish(tool_calls, &context),
            BudgetDecision::Continue
        );
    }
    assert!(counters.cumulative_input_tokens > 0);
    assert!(counters.active_elapsed() >= Duration::from_secs(10 * 60 * 60));
    assert_no_event(&mut receiver);
}

#[tokio::test(start_paused = true)]
async fn default_settings_still_stop_at_the_existing_caps() {
    // Given: the unchanged default settings (400 calls, 2 hours, no token cap).
    let bus = EventBus::new(16);
    let settings = BudgetSettings::default();
    let context = context(&bus, &settings);
    let mut counters = BudgetCounters::default();
    // Then: 399 calls continue and the 400th stops.
    assert_eq!(counters.publish(399, &context), BudgetDecision::Continue);
    assert!(matches!(
        counters.publish(400, &context),
        BudgetDecision::Exhausted(breach) if breach.detail.contains("max_tool_calls=400")
    ));
    // Given: a fresh run that has used exactly two hours, then a bit more.
    let mut counters = BudgetCounters::default();
    tokio::time::advance(Duration::from_secs(7_200)).await;
    assert_eq!(counters.publish(0, &context), BudgetDecision::Continue);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(matches!(
        counters.publish(0, &context),
        BudgetDecision::Exhausted(breach) if breach.detail.contains("max_elapsed_ms=7200000")
    ));
    // Token usage alone never stops a default run.
    let mut counters = BudgetCounters::default();
    counters.usage(Usage {
        input_tokens: u64::MAX,
        output_tokens: u64::MAX,
        ..Default::default()
    });
    assert_eq!(counters.publish(0, &context), BudgetDecision::Continue);
}

#[tokio::test]
async fn unlimited_tool_calls_warn_only_on_the_token_axis() {
    // Given: unlimited tool calls with a bounded token cap near its end.
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    let settings = BudgetSettings {
        max_tokens: ResourceLimit::Bounded(100),
        ..BudgetSettings::lane_scope()
    };
    let context = context(&bus, &settings);
    let mut counters = BudgetCounters::default();
    // When: tool calls are high but tokens are not near the cap.
    assert_eq!(counters.publish(5_001, &context), BudgetDecision::Continue);
    assert_no_event(&mut receiver);
    counters.usage(Usage {
        input_tokens: 90,
        ..Default::default()
    });
    assert_eq!(counters.publish(5_001, &context), BudgetDecision::Continue);
    // Then: the warning reports tool calls as unlimited.
    let event = receiver.recv().await.expect("warning");
    assert!(matches!(event.kind, event_bus::EventKind::Diagnostic(d)
        if d.code == diagnostic_codes::BUDGET_WARNING
            && d.detail.contains("remaining_tool_calls=unlimited")
            && d.detail.contains("remaining_tokens=10")));
}

#[tokio::test(start_paused = true)]
async fn runaway_diagnostics_still_fire_with_unlimited_resources() {
    // Given: lane-scope settings with small diagnostic thresholds.
    let bus = EventBus::new(16);
    let settings = BudgetSettings {
        max_no_progress_rounds: 2,
        max_file_rereads: 1,
        ..BudgetSettings::lane_scope()
    };
    let context = context(&bus, &settings);
    // When: consecutive rounds make no progress.
    let mut counters = BudgetCounters::default();
    tokio::time::advance(Duration::from_secs(10 * 60 * 60)).await;
    for _ in 0..3 {
        counters.finish_round();
    }
    // Then: NoProgress stops the run despite unlimited resources.
    assert!(matches!(
        counters.publish(1_000, &context),
        BudgetDecision::Exhausted(breach)
            if breach.code == diagnostic_codes::NO_PROGRESS
                && breach.detail.contains("max_no_progress_rounds=2")
    ));
    // When: the same file is reread more than allowed.
    let mut counters = BudgetCounters::default();
    for _ in 0..3 {
        counters.read(Path::new("reread.txt"));
    }
    // Then: the reread guard stops the run.
    assert!(matches!(
        counters.publish(1_000, &context),
        BudgetDecision::Exhausted(breach)
            if breach.code == diagnostic_codes::BUDGET_EXHAUSTED
                && breach.detail.contains("max_file_rereads=1")
    ));
}

#[test]
fn lane_and_child_share_identity_aware_credits_across_snapshot_restore() {
    let mut counters = BudgetCounters::default();
    for index in 0..MAX_UNOBSERVED_SPAWN_CREDITS {
        let counted = if index % 2 == 0 {
            counters.mark_lane_spawned(&format!("l{index}"))
        } else {
            counters.mark_spawned(&format!("run-{index}"))
        };
        assert!(counted);
    }
    assert!(!counters.mark_lane_spawned("twenty-first"));
    assert!(!counters.mark_spawned("run-extra"));
    counters.finish_round();
    counters.finish_round();
    assert_eq!(counters.no_progress_rounds, 1);
    assert!(counters.mark_lane_completed("l0"));
    assert!(counters.mark_spawned("run-new"));
    let json = serde_json::to_string(&counters.spawn_credits).unwrap();
    let mut restored = BudgetCounters {
        spawn_credits: serde_json::from_str(&json).unwrap(),
        ..Default::default()
    };
    assert!(!restored.mark_lane_completed("l0"));
    assert!(!restored.mark_lane_spawned("l0"));
    assert!(!restored.mark_spawned("run-new"));
    // Same ID space, but child and lane terminal observations cannot alias.
    assert!(restored.mark_child_completed("l0"));
}

#[test]
fn lane_credit_waits_for_full_visible_presentation_and_survives_snapshot() {
    let mut counters = BudgetCounters::default();
    assert!(counters.mark_lane_spawned("ready"));
    counters.queue_lane_presentation("full ready artifact", vec!["ready".into()]);
    assert_eq!(counters.spawn_credits.unobserved, 1);
    let persisted = serde_json::to_string(&counters.spawn_credits).unwrap();
    counters.spawn_credits = serde_json::from_str(&persisted).unwrap();
    assert!(counters.observe_lane_presentations(&[providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: "full ready artifact".into()
        }]
    }]));
    assert_eq!(counters.spawn_credits.unobserved, 0);
    assert!(!counters.mark_lane_completed("ready"));
    assert!(!counters.observe_lane_presentations(&[]));
    assert!(counters.mark_lane_spawned("compacted-away"));
    counters.queue_lane_presentation("full compacted artifact", vec!["compacted-away".into()]);
    assert!(counters.observe_lane_presentations(&[providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: "reference only".into()
        }]
    }]));
    assert_eq!(counters.spawn_credits.unobserved, 1);
    assert!(
        !counters
            .spawn_credits
            .observed
            .contains("lane:compacted-away")
    );
}

fn text_read_result(content: impl Into<String>) -> tools::ToolResult {
    let mut result = tools::ToolResult::success(content);
    result.detail = Some(serde_json::json!({"bytes_returned":result.content.len()}));
    result
}

#[test]
fn rereads_are_scoped_to_content_and_effective_range() {
    let bus = EventBus::new(16);
    let settings = BudgetSettings {
        max_file_rereads: 1,
        ..Default::default()
    };
    let ctx = context(&bus, &settings);
    for mode in ["range", "content"] {
        let mut counters = BudgetCounters::default();
        for n in 0..24 {
            let input = serde_json::json!({"offset":if mode=="range" {n+1} else {1}});
            let result = text_read_result(if mode == "content" {
                format!("revision-{n}")
            } else {
                "same".into()
            });
            counters.read_result(Path::new("file.txt"), &input, &result);
            counters.mark_read_progress();
            assert_eq!(
                counters.publish(n + 1, &ctx),
                BudgetDecision::Continue,
                "{mode}:{n}"
            );
        }
    }
}
#[test]
fn read_success_and_equivalent_defaults_do_not_forgive_rereads() {
    let bus = EventBus::new(16);
    let settings = BudgetSettings {
        max_file_rereads: 1,
        ..Default::default()
    };
    let ctx = context(&bus, &settings);
    let mut counters = BudgetCounters::default();
    let result = text_read_result("same content");
    for input in [
        serde_json::json!({}),
        serde_json::json!({"offset":1,"limit":999,"byte_offset":0}),
    ] {
        counters.read_result(Path::new("file.txt"), &input, &result);
        counters.mark_read_progress();
        assert_eq!(counters.publish(1, &ctx), BudgetDecision::Continue);
    }
    counters.read_result(Path::new("file.txt"), &serde_json::json!({}), &result);
    counters.mark_read_progress();
    assert!(
        matches!(counters.publish(1,&ctx),BudgetDecision::Exhausted(b) if b.detail.contains("max_file_rereads=1"))
    );
}

#[test]
fn unrelated_progress_and_interface_switching_do_not_forgive_rereads() {
    let bus = EventBus::new(16);
    let settings = BudgetSettings {
        max_file_rereads: 1,
        ..Default::default()
    };
    let ctx = context(&bus, &settings);
    for first_direct in [false, true] {
        let mut counters = BudgetCounters::default();
        for n in 0..3 {
            if (n % 2 == 0) == first_direct {
                counters.read_result(
                    Path::new("file.txt"),
                    &serde_json::json!({}),
                    &text_read_result("same content"),
                );
            } else {
                counters.read(Path::new("file.txt"));
            }
            counters.mark_progress(); // e.g. a successful grep; changed no file.
            let decision = counters.publish(1, &ctx);
            if n < 2 {
                assert_eq!(decision, BudgetDecision::Continue);
            } else {
                assert!(matches!(decision, BudgetDecision::Exhausted(b)
                    if b.detail.contains("max_file_rereads=1")));
            }
        }
    }
}

#[test]
fn unclassified_reads_do_not_trust_changing_display_text() {
    let bus = EventBus::new(16);
    let settings = BudgetSettings {
        max_file_rereads: 1,
        ..Default::default()
    };
    let ctx = context(&bus, &settings);
    for metadata in ["missing", "artifact", "invalid-length", "invalid-boundary"] {
        let mut counters = BudgetCounters::default();
        for n in 0..3 {
            let mut result = tools::ToolResult::success(format!("é-{n}"));
            result.detail = match metadata {
                "missing" => None,
                "artifact" => {
                    Some(serde_json::json!({"output_artifact":{"path":format!("out-{n}")}}))
                }
                "invalid-length" => {
                    Some(serde_json::json!({"bytes_returned":result.content.len()+1}))
                }
                "invalid-boundary" => Some(serde_json::json!({"bytes_returned":1})),
                _ => unreachable!(),
            };
            counters.read_result(
                Path::new("file.txt"),
                &serde_json::json!({"offset":n+1,"limit":n+1}),
                &result,
            );
            counters.mark_read_progress();
            let decision = counters.publish(1, &ctx);
            if n < 2 {
                assert_eq!(decision, BudgetDecision::Continue, "{metadata}:{n}");
            } else {
                assert!(
                    matches!(decision, BudgetDecision::Exhausted(b)
                    if b.detail.contains("max_file_rereads=1")),
                    "{metadata}"
                );
            }
        }
    }
}

#[test]
fn uncollected_cohort_survives_unrelated_completion_rereads_and_snapshot() {
    let mut counters = BudgetCounters::default();
    let managed = BTreeSet::from(["A".into(), "B".into(), "C".into()]);
    assert!(
        !counters
            .spawn_credits
            .reconcile_uncollected_lanes(&managed, &managed)
    );
    for n in 0..99 {
        counters.spawn_credits.record_lane_open(&format!("new-{n}"));
        counters.mark_lane_spawned(&format!("new-{n}"));
    }
    assert_eq!(counters.spawn_credits.uncollected_lane_open_count, 99);
    assert!(
        !counters
            .spawn_credits
            .reconcile_uncollected_lanes(&managed, &managed)
    );
    assert!(counters.mark_lane_completed("A"));
    assert_eq!(counters.spawn_credits.unobserved, 0); // Existing all-credit refund.
    assert_eq!(
        counters.spawn_credits.uncollected_lane_ids,
        BTreeSet::from(["B".into(), "C".into()])
    );
    counters.spawn_credits.record_lane_open("hundredth");
    counters.spawn_credits.record_lane_open("hundredth"); // Intent-only retry.
    assert_eq!(counters.spawn_credits.uncollected_lane_open_count, 100);
    assert!(
        counters
            .spawn_credits
            .reconcile_uncollected_lanes(&managed, &managed)
    );
    assert!(counters.mark_lane_completed("small-new-result"));
    assert!(!counters.mark_lane_completed("A"));
    counters.mark_progress();
    let snapshot = serde_json::to_string(&counters.spawn_credits).unwrap();
    counters.spawn_credits = serde_json::from_str(&snapshot).unwrap();
    assert!(
        counters
            .spawn_credits
            .reconcile_uncollected_lanes(&managed, &managed)
    );
    assert_eq!(counters.spawn_credits.uncollected_lane_open_count, 100);
    assert!(counters.mark_lane_completed("B"));
    assert!(
        counters
            .spawn_credits
            .reconcile_uncollected_lanes(&managed, &managed)
    );
    assert_eq!(
        counters.spawn_credits.uncollected_lane_ids,
        BTreeSet::from(["C".into()])
    );
    assert!(counters.mark_lane_completed("C"));
    assert!(
        !counters
            .spawn_credits
            .reconcile_uncollected_lanes(&managed, &managed)
    );
    assert_eq!(counters.spawn_credits.uncollected_lane_open_count, 0);
}

#[test]
fn uncollected_running_and_management_end_are_not_completion() {
    let mut credits = SpawnCredits::default();
    let managed = BTreeSet::from(["old".into(), "new".into()]);
    let old = BTreeSet::from(["old".into()]);
    assert!(!credits.reconcile_uncollected_lanes(&managed, &old));
    for n in 0..100 {
        credits.record_lane_open(&format!("open-{n}"));
    }
    assert!(credits.reconcile_uncollected_lanes(&managed, &old));
    // A temporary Working report disables refusal, not the original cohort/count.
    assert!(!credits.reconcile_uncollected_lanes(&managed, &BTreeSet::new()));
    assert_eq!(credits.uncollected_lane_open_count, 100);
    assert!(credits.reconcile_uncollected_lanes(&managed, &old));
    // A newer terminal result does not replace the older cohort.
    assert!(!credits.reconcile_uncollected_lanes(&managed, &BTreeSet::from(["new".into()])));
    assert_eq!(credits.uncollected_lane_ids, old);
    credits.unobserved = 17;
    assert!(
        !credits.reconcile_uncollected_lanes(&BTreeSet::from(["new".into()]), &BTreeSet::new())
    );
    assert!(credits.uncollected_lane_ids.is_empty());
    assert_eq!(credits.unobserved, 17); // Detach never refunds common credit.
    assert_eq!(credits.uncollected_lane_open_count, 0);
    for n in 0..101 {
        credits.record_lane_open(&format!("running-{n}"));
    }
    assert_eq!(credits.uncollected_lane_open_count, 0);
}

#[test]
fn uncollected_old_json_defaults_but_invalid_values_fail() {
    let old = r#"{"spawned":[],"observed":[],"unobserved":20}"#;
    let credits: SpawnCredits = serde_json::from_str(old).unwrap();
    assert_eq!(credits.uncollected_lane_open_count, 0);
    assert!(credits.uncollected_lane_ids.is_empty());
    let mut value = serde_json::to_value(credits).unwrap();
    value["uncollected_lane_open_count"] = serde_json::json!(-1);
    assert!(serde_json::from_value::<SpawnCredits>(value).is_err());
}

#[test]
fn uncollected_tool_presentation_compacted_before_request_is_not_collection() {
    let mut budget = BudgetCounters::default();
    let cohort = BTreeSet::from(["artifact".into()]);
    budget
        .spawn_credits
        .reconcile_uncollected_lanes(&cohort, &cohort);
    for n in 0..100 {
        budget.spawn_credits.record_lane_open(&format!("open-{n}"));
    }
    budget.queue_lane_presentation("full tool report", vec!["artifact".into()]);
    let snapshot = serde_json::to_string(&budget.spawn_credits).unwrap();
    budget.spawn_credits = serde_json::from_str(&snapshot).unwrap();
    assert!(
        budget
            .spawn_credits
            .reconcile_uncollected_lanes(&cohort, &cohort)
    );
    // Only a compaction summary survives the provider boundary.
    budget.observe_lane_presentations(&[providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: "summary: reference to artifact".into(),
        }],
    }]);
    assert!(
        budget
            .spawn_credits
            .reconcile_uncollected_lanes(&cohort, &cohort)
    );
    assert!(!budget.spawn_credits.observed.contains("lane:artifact"));
    budget.queue_lane_presentation("full tool report", vec!["artifact".into()]);
    budget.observe_lane_presentations(&[providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::ToolResult {
            tool_call_id: "read-again".into(),
            content: vec![providers::ToolResultContent::Text {
                text: "full tool report".into(),
            }],
            is_error: false,
        }],
    }]);
    assert!(budget.spawn_credits.observed.contains("lane:artifact"));
    assert!(
        !budget
            .spawn_credits
            .reconcile_uncollected_lanes(&cohort, &cohort)
    );
}

#[test]
fn uncollected_reattach_requires_new_current_terminal_report_without_reviving_old_counts() {
    let mut credits = SpawnCredits::default();
    let ids = BTreeSet::from(["lane".into()]);
    credits.reconcile_uncollected_lanes(&ids, &ids);
    credits.record_lane_open("before-detach");
    credits.reconcile_uncollected_lanes(&BTreeSet::new(), &BTreeSet::new());
    assert_eq!(credits.uncollected_lane_open_count, 0);
    // A reattached lane with only the old revision has no effective terminal report.
    credits.reconcile_uncollected_lanes(&ids, &BTreeSet::new());
    credits.record_lane_open("after-reattach");
    assert!(credits.uncollected_lane_ids.is_empty());
    assert_eq!(credits.uncollected_lane_open_count, 0);
    credits.reconcile_uncollected_lanes(&ids, &ids);
    assert_eq!(credits.uncollected_lane_ids, ids);
    assert_eq!(credits.uncollected_lane_open_count, 0);
}
