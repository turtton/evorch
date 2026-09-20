use super::{TelemetryOverlay, TelemetryRow, ThreadMetrics, TokenUsage};
use std::time::{Duration, Instant};

impl TelemetryOverlay {
    pub fn thread_metrics(&self, run_ids: &[String]) -> ThreadMetrics {
        self.thread_metrics_at(run_ids, Instant::now())
    }

    pub fn thread_metrics_at(&self, run_ids: &[String], now: Instant) -> ThreadMetrics {
        let mut cost_total = 0.0;
        let mut has_cost = false;
        let mut usage = TokenUsage::default();
        let mut wall_time = Duration::ZERO;
        for run_id in run_ids {
            if let Some(cost) = self.costs.get(run_id) {
                cost_total += cost;
                has_cost = true;
            }
            if let Some(billed) = self.billed.get(run_id) {
                for entry in billed.values() {
                    usage.input = usage.input.saturating_add(entry.input);
                    usage.output = usage.output.saturating_add(entry.output);
                    usage.cache_read = usage.cache_read.saturating_add(entry.cache_read);
                    usage.cache_write = usage.cache_write.saturating_add(entry.cache_write);
                }
            }
            if let Some(accumulated) = self.accumulated_running.get(run_id) {
                wall_time += *accumulated;
            }
            if let Some(start) = self.active_running_start.get(run_id) {
                wall_time += now.saturating_duration_since(*start);
            }
        }
        let latest = run_ids
            .iter()
            .filter_map(|id| self.rows.get(id))
            .max_by_key(|row| row.context_order);
        ThreadMetrics {
            cost: has_cost.then_some(cost_total),
            cache_hit_rate: (usage.input > 0).then(|| usage.cache_hit_rate()),
            wall_time,
            context_pressure: latest.and_then(TelemetryRow::context_pressure),
            ttft: latest
                .and_then(TelemetryRow::average_ttft_ms)
                .map(Duration::from_millis),
            tok_s: latest.and_then(|row| row.tok_s_at(now)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_metrics_aggregates_latest_ttft_and_tok_s() {
        // Given: run IDs are not ordered by the latest telemetry observation.
        let now = Instant::now();
        let mut overlay = TelemetryOverlay::new();
        for (id, order, ttft, output) in [("latest", 2, 240, 80), ("older", 1, 900, 10)] {
            overlay.rows.insert(
                id.into(),
                TelemetryRow {
                    context_order: order,
                    ttft_ms: Some(ttft),
                    output_tokens: output,
                    request_duration: Some(Duration::from_secs(2)),
                    ..Default::default()
                },
            );
        }
        // When: aggregating only this thread's runs.
        let metrics = overlay.thread_metrics_at(&["latest".into(), "older".into()], now);
        // Then: latency and throughput come from the latest observed run.
        assert_eq!(metrics.ttft, Some(Duration::from_millis(240)));
        assert_eq!(metrics.tok_s, Some(40.0));
    }

    #[test]
    fn thread_metrics_preserves_missing_latest_measurements() {
        // Given: an older run has samples, but the newest request does not.
        let now = Instant::now();
        let mut overlay = TelemetryOverlay::new();
        overlay.rows.insert(
            "older".into(),
            TelemetryRow {
                context_order: 1,
                ttft_ms: Some(900),
                output_tokens: 100,
                request_duration: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        );
        overlay.rows.insert(
            "latest".into(),
            TelemetryRow {
                context_order: 2,
                request_started_at: Some(now),
                ..Default::default()
            },
        );
        // When: the thread metrics are computed before a duration is measurable.
        let metrics = overlay.thread_metrics_at(&["older".into(), "latest".into()], now);
        // Then: older measurements are not presented as the latest request.
        assert_eq!(metrics.ttft, None);
        assert_eq!(metrics.tok_s, None);
    }
}
