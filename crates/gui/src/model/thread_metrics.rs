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
            if let Some(done) = self.run_wall_time.get(run_id) {
                wall_time += *done;
            }
            if let Some(start) = self.run_started.get(run_id) {
                wall_time += now.saturating_duration_since(*start);
            }
        }
        ThreadMetrics {
            cost: has_cost.then_some(cost_total),
            cache_hit_rate: (usage.input > 0).then(|| usage.cache_hit_rate()),
            wall_time,
            context_pressure: run_ids
                .iter()
                .filter_map(|id| self.rows.get(id))
                .max_by_key(|row| row.context_order)
                .and_then(TelemetryRow::context_pressure),
        }
    }
}
