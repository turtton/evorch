use super::{TelemetryOverlay, TelemetryRow, ThreadMetrics, TokenUsage};
use std::time::{Duration, Instant};

impl ThreadMetrics {
    pub fn context_label(&self) -> String {
        match (self.context_used_tokens, self.context_pressure) {
            (Some(tokens), Some(pressure)) => format!("ctx {}K({pressure}%)", tokens / 1_000),
            _ => "ctx —".into(),
        }
    }

    pub fn ttft_label(&self) -> String {
        let current = self.ttft.map_or_else(
            || "TTFT —".into(),
            |ttft| format!("TTFT {}ms", ttft.as_millis()),
        );
        match self.average_ttft {
            Some(average) => format!("{current} (Δ{}ms)", average.as_millis()),
            None => current,
        }
    }

    pub fn tok_s_label(&self) -> String {
        let current = self
            .tok_s
            .map_or_else(|| "— tok/s".into(), |rate| format!("{rate:.1} tok/s"));
        match self.average_tok_s {
            Some(average) => format!("{current} (Δ{average:.1} tok/s)"),
            None => current,
        }
    }

    pub fn cache_hit_rate_label(&self) -> String {
        let current = self
            .cache_hit_rate
            .map_or_else(|| "cache —".into(), |rate| format!("cache {rate:.0}%"));
        match self.average_cache_hit_rate {
            Some(average) => format!("{current} (Δ{average:.0}%)"),
            None => current,
        }
    }
}

impl TelemetryOverlay {
    pub fn thread_metrics(&self, run_ids: &[String]) -> ThreadMetrics {
        self.thread_metrics_at(run_ids, Instant::now())
    }

    pub fn thread_metrics_at(&self, run_ids: &[String], now: Instant) -> ThreadMetrics {
        let mut cost_total = 0.0;
        let mut has_cost = false;
        let mut wall_time = Duration::ZERO;
        for run_id in run_ids {
            if let Some(cost) = self.costs.get(run_id) {
                cost_total += cost;
                has_cost = true;
            }
            if let Some(accumulated) = self.accumulated_running.get(run_id) {
                wall_time += *accumulated;
            }
            if let Some(start) = self.active_running_start.get(run_id) {
                wall_time += now.saturating_duration_since(*start);
            }
        }
        // Prefer explicit conversation roots. Older observations can lack a
        // lifecycle marker, so use parentless runs only when none are known.
        // A thread still owns all its subagents for the total cost above.
        let mut root_ids: Vec<_> = run_ids
            .iter()
            .filter(|id| self.rows.get(*id).is_some_and(|row| row.conversation_root))
            .collect();
        if root_ids.is_empty() {
            root_ids = run_ids
                .iter()
                .filter(|id| {
                    self.rows
                        .get(*id)
                        .is_some_and(|row| row.parent_run_id.is_none())
                })
                .collect();
        }
        // A single focused subagent pane passes only its own run ID. Keep its
        // request metrics available even when its parent is outside the slice.
        let roots: Vec<_> = if root_ids.is_empty() {
            run_ids
                .iter()
                .filter_map(|id| self.rows.get(id))
                .filter(|row| {
                    row.parent_run_id
                        .as_ref()
                        .is_none_or(|parent| !run_ids.contains(parent))
                })
                .collect()
        } else {
            root_ids
                .iter()
                .filter_map(|id| self.rows.get(*id))
                .collect()
        };
        let mut conversation_cost_total = 0.0;
        let mut has_conversation_cost = false;
        for run_id in &root_ids {
            if let Some(cost) = self.costs.get(*run_id) {
                conversation_cost_total += cost;
                has_conversation_cost = true;
            }
        }
        let mut usage = TokenUsage::default();
        let mut has_usage = false;
        let mut ttft_sum_ms = 0_u64;
        let mut ttft_count = 0_u64;
        let mut provider_duration = Duration::ZERO;
        for row in roots.iter().copied() {
            has_usage |= row.latest_context.is_some();
            usage.input = usage.input.saturating_add(row.usage.input);
            usage.output = usage.output.saturating_add(row.usage.output);
            ttft_sum_ms = ttft_sum_ms.saturating_add(row.ttft_sum_ms);
            ttft_count = ttft_count.saturating_add(row.ttft_count);
            provider_duration = provider_duration.saturating_add(row.completed_request_duration);
            usage.cache_read = usage.cache_read.saturating_add(row.usage.cache_read);
        }
        let latest = roots.into_iter().max_by_key(|row| row.context_order);
        ThreadMetrics {
            cost: has_cost.then_some(cost_total),
            conversation_cost: has_conversation_cost.then_some(conversation_cost_total),
            cache_hit_rate: latest
                .and_then(|row| row.latest_context.as_ref())
                .map(|request| request.usage.cache_hit_rate()),
            average_cache_hit_rate: has_usage.then(|| usage.cache_hit_rate()),
            wall_time,
            context_pressure: latest.and_then(TelemetryRow::context_pressure),
            context_used_tokens: latest.and_then(TelemetryRow::context_used_tokens),
            ttft: latest
                .and_then(TelemetryRow::latest_ttft_ms)
                .map(Duration::from_millis),
            average_ttft: (ttft_count > 0).then(|| Duration::from_millis(ttft_sum_ms / ttft_count)),
            tok_s: latest.and_then(|row| row.tok_s_at(now)),
            // RequestCompleted has provider duration, which excludes tool execution
            // and idle time between requests. Streaming estimates never enter this sum.
            average_tok_s: (!provider_duration.is_zero()).then(|| {
                Duration::from_secs(usage.output).as_secs_f64() / provider_duration.as_secs_f64()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_label_truncates_to_whole_thousands() {
        let metrics = ThreadMetrics {
            context_used_tokens: Some(999),
            context_pressure: Some(1),
            ..Default::default()
        };
        assert_eq!(metrics.context_label(), "ctx 0K(1%)");
        let metrics = ThreadMetrics {
            context_used_tokens: Some(200_999),
            context_pressure: Some(20),
            ..Default::default()
        };
        assert_eq!(metrics.context_label(), "ctx 200K(20%)");
    }

    #[test]
    fn conversation_cost_includes_continuations_but_excludes_children() {
        let mut overlay = TelemetryOverlay::new();
        overlay.rows.insert(
            "first".into(),
            TelemetryRow {
                conversation_root: true,
                ..Default::default()
            },
        );
        overlay.rows.insert(
            "continued".into(),
            TelemetryRow {
                conversation_root: true,
                parent_run_id: Some("first".into()),
                ..Default::default()
            },
        );
        overlay.rows.insert(
            "child".into(),
            TelemetryRow {
                parent_run_id: Some("first".into()),
                ..Default::default()
            },
        );
        overlay.costs.insert("first".into(), 1.0);
        overlay.costs.insert("continued".into(), 2.0);
        overlay.costs.insert("child".into(), 3.0);
        let metrics = overlay.thread_metrics(&["child".into(), "first".into(), "continued".into()]);
        assert_eq!(metrics.conversation_cost, Some(3.0));
        assert_eq!(metrics.cost, Some(6.0));
    }

    #[test]
    fn orphan_child_cost_never_becomes_conversation_cost() {
        let mut overlay = TelemetryOverlay::new();
        overlay.rows.insert(
            "child".into(),
            TelemetryRow {
                parent_run_id: Some("outside".into()),
                ..Default::default()
            },
        );
        overlay.costs.insert("child".into(), 3.0);
        let metrics = overlay.thread_metrics(&["child".into()]);
        assert_eq!(metrics.cost, Some(3.0));
        assert_eq!(metrics.conversation_cost, None);
    }

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
