use std::time::{Duration, Instant};

use config::types::provider::ModelPricing;
use gui::model::telemetry::{TelemetryRow, TokenUsage};

fn usage() -> TokenUsage {
    TokenUsage {
        input: 20_000,
        output: 15_400,
        cache_read: 78_300,
        cache_write: 1_700,
    }
}

#[test]
fn cache_hit_rate_uses_senpi_formula() {
    // Given: output is excluded from the input-side cache denominator.
    let usage = usage();
    // When / Then: percentage and the empty denominator are well defined.
    assert!((usage.cache_hit_rate() - 78.3).abs() < 1e-10);
    assert_eq!(TokenUsage::default().cache_hit_rate(), 0.0);
}

#[test]
fn cost_hidden_when_pricing_unknown() {
    // Given: completed usage without known prices.
    let mut row = TelemetryRow::default();
    row.usage = usage();
    // When / Then: unknown does not mean free.
    assert_eq!(row.usage.estimated_cost(None), None);
    assert_eq!(
        row.compact_line_at(Instant::now(), None),
        "115.4K tok · cache 78.3%"
    );
}

#[test]
fn cache_segment_hidden_below_10_percent() {
    // Given: a rate strictly below the threshold.
    let mut row = TelemetryRow::default();
    row.usage = TokenUsage {
        input: 901,
        cache_read: 99,
        ..TokenUsage::default()
    };
    // When / Then: threshold uses unrounded percentages.
    assert_eq!(row.compact_line_at(Instant::now(), None), "1.0K tok");
    row.usage.input = 900;
    row.usage.cache_read = 100;
    assert_eq!(
        row.compact_line_at(Instant::now(), None),
        "1.0K tok · cache 10.0%"
    );
}

#[test]
fn compact_line_format() {
    // Given: finalized speed and an estimated price from completed token counts.
    let mut row = TelemetryRow::default();
    row.usage = usage();
    row.output_tokens = 226;
    row.request_duration = Some(Duration::from_secs(5));
    let pricing = ModelPricing {
        input: Some(0.5),
        output: Some(2.0),
        cache_read: Some(0.01),
        cache_write: Some(0.25),
    };
    // When / Then: stable compact presentation, with no estimated token speed marker.
    assert_eq!(
        row.compact_line_at(Instant::now(), Some(pricing)),
        "$0.042 · 115.4K tok · 45.2 tok/s · cache 78.3%"
    );
}
