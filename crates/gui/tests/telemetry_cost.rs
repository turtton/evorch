use std::time::{Duration, Instant};

use config::types::provider::ModelPricing;
use gui::model::telemetry::TelemetryRow;
use recorder::pricing::TokenUsage;

fn usage() -> TokenUsage {
    TokenUsage {
        input: 98_300,
        output: 15_400,
        cache_read: 78_300,
        cache_write: 1_700,
    }
}

#[test]
fn cost_hidden_when_pricing_unknown() {
    // Given: completed usage without known prices.
    let mut row = TelemetryRow::default();
    row.usage = usage();
    // When / Then: unknown does not mean free, and billed cache is left to the retention tooltip.
    assert_eq!(row.usage.estimated_cost(None), None);
    assert_eq!(row.compact_line_at(Instant::now(), None), "113.7K tok");
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
        "$0.041 · 113.7K tok · 45.2 tok/s"
    );
}
