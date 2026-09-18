use std::time::{Duration, Instant};

use config::types::provider::ModelPricing;
use gui::model::telemetry::{TelemetryRow, TokenUsage};

fn usage() -> TokenUsage {
    TokenUsage {
        input: 98_300,
        output: 15_400,
        cache_read: 78_300,
        cache_write: 1_700,
    }
}

#[test]
fn cache_hit_rate_when_input_includes_cached_tokens() {
    // Given: a warm turn whose input already includes cache reads.
    let usage = TokenUsage {
        input: 42_837,
        cache_read: 42_496,
        ..TokenUsage::default()
    };
    // When: computing the percentage.
    let rate = usage.cache_hit_rate();
    // Then: nearly all input tokens hit the cache.
    assert!((rate - 99.2).abs() < 0.05, "rate={rate}");
}

#[test]
fn cache_hit_rate_when_counts_are_at_boundaries() {
    for (input, cache_read, expected) in [(100, 0, 0.0), (0, 10, 0.0), (10, 20, 100.0)] {
        // Given: no hits, no input, or inconsistent cached counts.
        let usage = TokenUsage {
            input,
            cache_read,
            ..TokenUsage::default()
        };
        // When: computing the percentage.
        let rate = usage.cache_hit_rate();
        // Then: zero-safe and clamped to 100 percent.
        assert_eq!(rate, expected, "input={input}, cache_read={cache_read}");
    }
}

#[test]
fn cost_when_cached_tokens_are_already_in_input() {
    // Given: distinct prices in USD per million tokens, including cache writes.
    let usage = TokenUsage {
        input: 1_000_000,
        output: 250_000,
        cache_read: 750_000,
        cache_write: 125_000,
    };
    let pricing = ModelPricing {
        input: Some(2.0),
        output: Some(4.0),
        cache_read: Some(0.5),
        cache_write: Some(1.0),
    };
    // When: estimating cost.
    let cost = usage.estimated_cost(Some(pricing));
    // Then: 0.5 uncached + 1 output + 0.375 read + 0.125 write.
    assert_eq!(cost, Some(2.0));
}

#[test]
fn cost_when_cache_read_exceeds_input_saturates_full_price_bucket() {
    // Given: inconsistent counts must not underflow the full-price bucket.
    let usage = TokenUsage {
        input: 1,
        cache_read: 1_000_000,
        ..TokenUsage::default()
    };
    let pricing = ModelPricing {
        input: Some(2.0),
        cache_read: Some(0.5),
        ..ModelPricing::default()
    };
    // When: estimating cost.
    let cost = usage.estimated_cost(Some(pricing));
    // Then: only cache-read charges remain.
    assert_eq!(cost, Some(0.5));
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
        "193.7K tok · cache 79.7%"
    );
}

#[test]
fn cache_segment_hidden_below_10_percent() {
    // Given: a rate strictly below the threshold.
    let mut row = TelemetryRow::default();
    row.usage = TokenUsage {
        input: 1000,
        cache_read: 99,
        ..TokenUsage::default()
    };
    // When / Then: threshold uses unrounded percentages.
    assert_eq!(row.compact_line_at(Instant::now(), None), "1.1K tok");
    row.usage.cache_read = 100;
    assert_eq!(
        row.compact_line_at(Instant::now(), None),
        "1.1K tok · cache 10.0%"
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
        "$0.042 · 193.7K tok · 45.2 tok/s · cache 79.7%"
    );
}
