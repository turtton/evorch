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

/// Counts the frames of a measurement window by their `seq`.
///
/// The first frame observed fixes the start of the window (its time is the
/// window's origin), so it is a baseline and is not a delivery of the window:
/// counting it would add one frame to a window that began on it.
#[derive(Debug, Default)]
pub struct Window {
    baseline: Option<u64>,
    last: u64,
    pub frames: u64,
    /// Sequence numbers that never arrived after the baseline.
    pub gaps: u64,
}

impl Window {
    /// Returns whether the frame counts as a delivery of the window.
    pub fn observe(&mut self, seq: u64) -> bool {
        if self.baseline.is_none() {
            self.baseline = Some(seq);
            self.last = seq;
            return false;
        }
        self.frames += 1;
        if seq > self.last + 1 {
            self.gaps += seq - self.last - 1;
        }
        self.last = seq;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.50), 50.0);
        assert_eq!(percentile(&v, 0.95), 95.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
        assert_eq!(percentile(&[7.0], 0.95), 7.0);
        assert!(percentile(&[], 0.5).is_nan());
    }

    #[test]
    fn dist_drops_non_finite_and_reports_n() {
        let d = dist(vec![3.0, f64::NAN, 1.0, 2.0, f64::INFINITY]);
        assert_eq!(d["n"], 3);
        assert_eq!(d["min"], 1.0);
        assert_eq!(d["max"], 3.0);
        assert_eq!(d["p50"], 2.0);
        assert!(dist(vec![]).is_null());
    }

    #[test]
    fn window_excludes_the_baseline_frame() {
        // 30 fps for 2 s fixes the window on one frame and delivers 60 more:
        // the rate is 60 / 2 s, not 61 / 2 s.
        let mut window = Window::default();
        assert!(!window.observe(100));
        for seq in 101..=160 {
            assert!(window.observe(seq));
        }
        assert_eq!(window.frames, 60);
        assert_eq!(window.gaps, 0);
        assert_eq!(window.frames as f64 / 2.0, 30.0);
    }

    #[test]
    fn window_counts_lost_sequence_numbers() {
        let mut window = Window::default();
        window.observe(10);
        window.observe(11);
        window.observe(14); // 12 and 13 never arrived
        window.observe(15);
        assert_eq!((window.frames, window.gaps), (3, 2));
    }

    #[test]
    fn window_with_only_the_baseline_has_no_frames() {
        let mut window = Window::default();
        window.observe(5);
        assert_eq!((window.frames, window.gaps), (0, 0));
    }
}
