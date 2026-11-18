//! Percentiles over raw samples.

use serde_json::{Value, json};

/// Nearest-rank percentile of an ascending slice.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// min / p50 / p95 / p99 / max / mean of `values`, or `null` when empty.
pub fn dist(mut values: Vec<f64>) -> Value {
    values.retain(|v| v.is_finite());
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    json!({
        "n": values.len(),
        "min": round(values[0]),
        "p50": round(percentile(&values, 0.50)),
        "p95": round(percentile(&values, 0.95)),
        "p99": round(percentile(&values, 0.99)),
        "max": round(values[values.len() - 1]),
        "mean": round(mean),
    })
}

pub fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}
