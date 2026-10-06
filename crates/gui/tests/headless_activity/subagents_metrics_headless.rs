use config::{Config, ModelEntryConfig, ProviderProfileConfig};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{
    AgentRunPhase, CacheBaselineMissing, CacheComparison, Event, LifecycleEvent, ProviderEvent,
};
use gui::model::{
    provider_settings::ProviderSettingsModel,
    tasks::{AgentRunSource, TasksModel},
    telemetry::TelemetryOverlay,
};
use runtime::{AgentSummary, RunId};

#[derive(Clone)]
struct Source;

impl AgentRunSource for Source {
    fn list(&self) -> Vec<AgentSummary> {
        [1, 2]
            .into_iter()
            .map(|id| AgentSummary {
                run_id: RunId::new(id),
                parent_run_id: Some(RunId::new(id - 1)),
                name: format!("worker-{id}"),
                role_name: "worker".into(),
                phase: AgentRunPhase::Done,
                model: "model".into(),
                category: None,
            })
            .collect()
    }
}

fn request(
    telemetry: &mut TelemetryOverlay,
    run: &str,
    input: u64,
    // Cache read tokens and the compared previous request's cache, if any.
    (cached, previous_cache): (u64, Option<u64>),
    output: u64,
    ttft_ms: u64,
    duration_ms: u64,
) {
    telemetry.apply_event(&Event::new(ProviderEvent::FirstTokenObserved {
        request_id: format!("{run}-{ttft_ms}"),
        provider: "local".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "model".into(),
        ttft_ms,
        run_id: Some(run.into()),
    }));
    telemetry.apply_event(&Event::new(ProviderEvent::CacheReuseObserved {
        request_id: format!("{run}-{ttft_ms}"),
        cache_read_tokens: cached,
        comparison: match previous_cache {
            Some(previous_cache_tokens) => CacheComparison::Compared {
                previous_request_id: format!("{run}-previous"),
                previous_cache_tokens,
            },
            None => CacheComparison::NoBaseline {
                reason: CacheBaselineMissing::NoPreviousRequest,
            },
        },
        run_id: Some(run.into()),
    }));
    telemetry.apply_event(&Event::new(ProviderEvent::RequestCompleted {
        request_id: format!("{run}-{ttft_ms}"),
        provider: "local".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "model".into(),
        streaming: true,
        duration_ms,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        finish_reason: "tool_use".into(),
        run_id: Some(run.into()),
        purpose: None,
        reasoning_tokens: None,
    }));
}

fn harness(telemetry: TelemetryOverlay) -> Harness<'static> {
    let mut tasks = TasksModel::new(Source);
    tasks.refresh();
    Harness::builder()
        .with_size(egui::vec2(500.0, 800.0))
        .build_ui(move |ui| {
            gui::theme::install(ui.ctx());
            gui::panes::agents::subagents_pane(
                ui,
                &tasks,
                &telemetry,
                &Default::default(),
                &["run-1".into(), "run-2".into()],
            );
        })
}

#[test]
fn subagents_cards_show_only_their_own_cost_cache_and_request_averages() {
    let mut telemetry = TelemetryOverlay::new();
    for (run, parent) in [("run-1", Some("run-0")), ("run-2", Some("run-1"))] {
        telemetry.apply_event(&Event::new(LifecycleEvent::AgentRunStarted {
            run_id: run.into(),
            parent_run_id: parent.map(str::to_owned),
            agent_name: "worker".into(),
            role: "worker".into(),
        }));
    }
    // run-1 warms up (billed 0% then 100%); run-2 reads half of its previous cache.
    request(&mut telemetry, "run-1", 1_000, (0, None), 80, 100, 1_000);
    request(
        &mut telemetry,
        "run-1",
        3_000,
        (3_000, Some(1_000)),
        60,
        500,
        3_000,
    );
    request(
        &mut telemetry,
        "run-2",
        10_000,
        (10_000, Some(20_000)),
        1_000,
        900,
        2_000,
    );
    let mut config = Config::default();
    let mut model = ModelEntryConfig::enabled("model");
    model.input_price = Some(100.0);
    model.output_price = Some(100.0);
    model.cache_read_price = Some(100.0);
    config.providers.insert(
        "local".into(),
        ProviderProfileConfig {
            models: vec![model],
            ..Default::default()
        },
    );
    telemetry.refresh_costs(&ProviderSettingsModel::seed_from_config(&config));

    let mut harness = harness(telemetry);
    harness.run_steps(4);
    // Each metric sits under its own card label and above the other card, whichever is first.
    let top = |label: &str| harness.get_by_label(label).rect().top();
    let in_card = |label: &str, card: &str, other: &str| {
        let metric = harness.get_by_label(label).rect();
        metric.top() > top(card) && (top(other) < top(card) || metric.bottom() < top(other))
    };
    for label in [
        "$0.414",
        "avg cache 100.0%",
        "avg 35.0 tok/s",
        "avg TTFT 300ms",
    ] {
        assert!(in_card(label, "worker-1", "worker-2"), "{label}");
    }
    for label in [
        "$1.100",
        "avg cache 50.0%",
        "avg 500.0 tok/s",
        "avg TTFT 900ms",
    ] {
        assert!(in_card(label, "worker-2", "worker-1"), "{label}");
    }
}

#[test]
fn subagents_cards_keep_unknown_metrics_distinct_from_zero() {
    let mut harness = harness(TelemetryOverlay::new());
    harness.run_steps(4);
    for label in ["$—", "avg cache —", "avg — tok/s", "avg TTFT —"] {
        assert_eq!(harness.query_all_by_label(label).count(), 2, "{label}");
    }
}
