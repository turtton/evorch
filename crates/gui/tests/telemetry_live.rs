use std::time::{Duration, Instant};

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, MessageEvent, ProviderEvent, UsageEvent};

#[test]
fn usage_event_without_run_id_is_ignored() {
    // Given: usage has no run correlation.
    let mut overlay = TelemetryOverlay::new();
    // When: usage is received.
    overlay.apply_event(&Event::new(UsageEvent::Usage {
        provider: "provider-a".into(),
        model: "model-a".into(),
        input_tokens: 10,
        output_tokens: 20,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
    }));
    // Then: no run is inferred.
    assert!(overlay.row("run-1").is_none());
}

#[test]
fn missing_fields_stay_none() {
    // Given: a completion without a preceding start.
    let mut overlay = TelemetryOverlay::new();
    // When: the completion is received.
    overlay.apply_event(&completed(10, 2));
    let mut uncorrelated = started();
    if let event_bus::EventKind::Provider(ProviderEvent::RequestStarted { run_id, .. }) =
        &mut uncorrelated.kind
    {
        *run_id = None;
    }
    overlay.apply_event(&uncorrelated);
    // Then: identity is not fabricated.
    let row = overlay.row("run-1").expect("row");
    assert!(row.provider.is_none());
    assert!(row.model.is_none());
    assert!(row.current_tool.is_none());
}
use gui::model::tasks::{AgentRunSource, TasksModel};
use gui::model::telemetry::TelemetryOverlay;
use runtime::{AgentSummary, RunId};

fn started() -> Event {
    Event::new(ProviderEvent::RequestStarted {
        request_id: "request-1".into(),
        provider: "fixture".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "model".into(),
        streaming: true,
        run_id: Some("run-1".into()),
    })
}

fn first_token() -> Event {
    Event::new(ProviderEvent::FirstTokenObserved {
        request_id: "request-1".into(),
        provider: "fixture".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "model".into(),
        ttft_ms: 800,
        run_id: Some("run-1".into()),
    })
}

fn completed(duration_ms: u64, output_tokens: u64) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: "request-1".into(),
        provider: "fixture".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "model".into(),
        streaming: true,
        duration_ms,
        input_tokens: 10,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some("run-1".into()),
    })
}

#[test]
fn request_started_records_start_and_elapsed_advances() {
    // Given: an injected monotonic clock.
    let now = Instant::now();
    let mut telemetry = TelemetryOverlay::new();
    // When: a request starts.
    telemetry.apply_event_at(&started(), now);
    // Then: elapsed is derived from the caller's clock, without sleeping.
    let row = telemetry.row("run-1").expect("row");
    assert_eq!(row.request_started_at, Some(now));
    assert_eq!(row.elapsed_at(now), Some(Duration::ZERO));
    assert_eq!(
        row.elapsed_at(now + Duration::from_millis(12_300)),
        Some(Duration::from_millis(12_300))
    );
}

#[test]
fn first_token_observed_sets_ttft() {
    // Given: a running request.
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&started());
    // When: the provider observes the first token.
    telemetry.apply_event(&first_token());
    // Then: the provider's measured TTFT is preserved.
    assert_eq!(telemetry.row("run-1").expect("row").ttft_ms, Some(800));
}

#[test]
fn provisional_tok_s_replaced_by_final_on_request_completed() {
    // Given: 80 streamed characters estimate 20 tokens over two seconds.
    let now = Instant::now();
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event_at(&started(), now);
    telemetry.apply_event_at(
        &Event::new(MessageEvent::MessageDelta {
            delta: "a".repeat(80),
            run_id: Some("run-1".into()),
        }),
        now + Duration::from_secs(1),
    );
    assert_eq!(
        telemetry
            .row("run-1")
            .expect("row")
            .tok_s_at(now + Duration::from_secs(2)),
        Some(10.0)
    );
    // When: the provider completes with 226 actual tokens in five seconds.
    telemetry.apply_event_at(&completed(5_000, 226), now + Duration::from_secs(9));
    // Then: final speed uses provider duration, not the GUI clock or estimate.
    let row = telemetry.row("run-1").expect("row");
    assert_eq!(row.tok_s_at(now + Duration::from_secs(20)), Some(45.2));
    assert_eq!(
        row.elapsed_at(now + Duration::from_secs(20)),
        Some(Duration::from_secs(5))
    );
    assert_eq!(row.usage.output, 226);
}

#[test]
fn new_request_resets_live_metrics_without_resetting_usage() {
    // Given: a completed request in the same run.
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&started());
    telemetry.apply_event(&first_token());
    telemetry.apply_event(&completed(5_000, 226));
    let now = Instant::now();
    // When: the next request starts.
    telemetry.apply_event_at(&started(), now);
    // Then: live observations reset, while billed usage remains cumulative.
    let row = telemetry.row("run-1").expect("row");
    assert_eq!(row.ttft_ms, None);
    assert_eq!(row.tok_s_at(now), None);
    assert_eq!(row.tok_s_at(now + Duration::from_secs(1)), Some(0.0));
    assert_eq!(row.usage.output, 226);
}

#[test]
fn zero_duration_has_no_infinite_rate() {
    // Given: a provider reports a zero duration.
    let mut telemetry = TelemetryOverlay::new();
    // When: the completion is folded.
    telemetry.apply_event(&completed(0, 100));
    // Then: rate is unavailable rather than infinite.
    assert_eq!(
        telemetry
            .row("run-1")
            .expect("row")
            .tok_s_at(Instant::now()),
        None
    );
}

struct Source;
impl AgentRunSource for Source {
    fn list(&self) -> Vec<AgentSummary> {
        vec![AgentSummary {
            run_id: RunId::new(1),
            name: "worker".into(),
            role_name: "worker".into(),
            phase: AgentRunPhase::Running,
            model: "model".into(),
        }]
    }
}

#[test]
fn completed_metrics_render_as_headless_labels() {
    // Given: a real Agents pane backed by completed telemetry.
    let mut tasks = TasksModel::new(Source);
    tasks.refresh();
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&started());
    telemetry.apply_event(&first_token());
    telemetry.apply_event(&completed(12_300, 556));
    let mut harness = Harness::new_ui(move |ui| {
        gui::panes::agents::agents_pane(ui, &tasks, &telemetry);
    });
    // When: egui renders its real widgets.
    harness.run();
    // Then: all three measured labels are accessible.
    for label in ["45.2 tok/s", "TTFT 0.8s", "Δ 12.3s"] {
        assert!(harness.query_by_label(label).is_some(), "missing {label}");
    }
}
