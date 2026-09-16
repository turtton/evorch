//! run ID が明示された観測イベントだけを集約する telemetry overlay。
//!
//! `UsageEvent::Usage` は `run_id` を持たないため常に無視する。表示用 token 数は
//! `ProviderEvent::RequestCompleted` の値だけを使い、未知の provider/model は推測せず
//! `None` のまま保持する。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, MessageEvent, ProviderEvent, ToolEvent};

#[path = "pricing.rs"]
pub mod pricing;

#[path = "quota.rs"]
pub mod quota;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryRow {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub current_tool: Option<String>,
    pub usage: TokenUsage,
    pub requests: u32,
    pub last_finish_reason: Option<String>,
    pub request_started_at: Option<Instant>,
    pub ttft_ms: Option<u64>,
    pub request_duration: Option<Duration>,
    pub output_tokens: u64,
    streamed_chars: u64,
}

impl TelemetryRow {
    pub fn elapsed_at(&self, now: Instant) -> Option<Duration> {
        self.request_duration.or_else(|| {
            self.request_started_at
                .map(|start| now.saturating_duration_since(start))
        })
    }

    pub fn tok_s_at(&self, now: Instant) -> Option<f64> {
        let elapsed = self.elapsed_at(now)?;
        // Duration converts the u64 count without a lossy integer narrowing cast.
        (!elapsed.is_zero())
            .then(|| Duration::from_secs(self.output_tokens).as_secs_f64() / elapsed.as_secs_f64())
    }
}

#[derive(Debug, Default)]
pub struct TelemetryOverlay {
    pub quota: quota::QuotaState,
    rows: BTreeMap<String, TelemetryRow>,
    billed: BTreeMap<String, BTreeMap<pricing::ModelKey, TokenUsage>>,
    costs: BTreeMap<String, f64>,
    run_started: BTreeMap<String, Instant>,
    run_wall_time: BTreeMap<String, Duration>,
}

/// スレッドに紐づく全 run の累計メトリクス。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ThreadMetrics {
    pub cost: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub wall_time: Duration,
}

impl TelemetryOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_event(&mut self, event: &Event) {
        self.apply_event_at(event, Instant::now());
    }

    pub fn apply_event_at(&mut self, event: &Event, now: Instant) {
        match &event.kind {
            EventKind::Provider(ProviderEvent::RequestStarted {
                provider,
                model,
                run_id: Some(run_id),
                ..
            }) => {
                let row = self.rows.entry(run_id.clone()).or_default();
                row.provider = Some(provider.clone());
                row.model = Some(model.clone());
                row.requests = row.requests.saturating_add(1);
                row.request_started_at = Some(now);
                row.request_duration = None;
                row.ttft_ms = None;
                row.output_tokens = 0;
                row.streamed_chars = 0;
            }
            EventKind::Provider(ProviderEvent::FirstTokenObserved {
                ttft_ms,
                run_id: Some(run_id),
                ..
            }) => {
                self.rows.entry(run_id.clone()).or_default().ttft_ms = Some(*ttft_ms);
            }
            EventKind::Message(
                MessageEvent::MessageDelta {
                    delta,
                    run_id: Some(run_id),
                }
                | MessageEvent::ReasoningDelta {
                    delta,
                    run_id: Some(run_id),
                },
            ) => {
                if let Some(row) = self.rows.get_mut(run_id)
                    && row.request_started_at.is_some()
                    && row.request_duration.is_none()
                {
                    // Deltas carry no token count: estimate across chunks, never bill this value.
                    row.streamed_chars = row.streamed_chars.saturating_add(
                        delta
                            .chars()
                            .fold(0_u64, |count, _| count.saturating_add(1)),
                    );
                    row.output_tokens = row.streamed_chars.div_ceil(4);
                }
            }
            EventKind::Provider(ProviderEvent::RequestCompleted {
                provider,
                profile,
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                finish_reason,
                duration_ms,
                run_id: Some(run_id),
                ..
            }) => {
                let usage = self
                    .billed
                    .entry(run_id.clone())
                    .or_default()
                    .entry(pricing::ModelKey {
                        provider: provider.clone(),
                        profile: profile.clone(),
                        model: model.clone(),
                    })
                    .or_default();
                usage.input = usage.input.saturating_add(*input_tokens);
                usage.output = usage.output.saturating_add(*output_tokens);
                usage.cache_read = usage.cache_read.saturating_add(*cache_read_tokens);
                usage.cache_write = usage.cache_write.saturating_add(*cache_write_tokens);
                let row = self.rows.entry(run_id.clone()).or_default();
                row.usage.input = row.usage.input.saturating_add(*input_tokens);
                row.usage.output = row.usage.output.saturating_add(*output_tokens);
                row.usage.cache_read = row.usage.cache_read.saturating_add(*cache_read_tokens);
                row.usage.cache_write = row.usage.cache_write.saturating_add(*cache_write_tokens);
                row.last_finish_reason = Some(finish_reason.clone());
                row.output_tokens = *output_tokens;
                row.request_duration = Some(Duration::from_millis(*duration_ms));
            }
            EventKind::Provider(ProviderEvent::RequestFailed {
                duration_ms,
                run_id: Some(run_id),
                ..
            }) => {
                let row = self.rows.entry(run_id.clone()).or_default();
                row.request_duration = Some(Duration::from_millis(*duration_ms));
                row.request_started_at = None;
                row.output_tokens = 0;
            }
            EventKind::Tool(ToolEvent::ToolStarted {
                tool_name,
                run_id: Some(run_id),
                ..
            }) => {
                self.rows.entry(run_id.clone()).or_default().current_tool = Some(tool_name.clone());
            }
            EventKind::Tool(ToolEvent::ToolCompleted {
                run_id: Some(run_id),
                ..
            }) => {
                self.rows.entry(run_id.clone()).or_default().current_tool = None;
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted { run_id, .. }) => {
                self.run_started.entry(run_id.clone()).or_insert(now);
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Done | AgentRunPhase::Error,
                ..
            }) => {
                if let Some(start) = self.run_started.remove(run_id) {
                    *self.run_wall_time.entry(run_id.clone()).or_default() +=
                        now.saturating_duration_since(start);
                }
            }
            EventKind::Lifecycle(_)
            | EventKind::Ledger(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::Snapshot(_) => {}
        }
    }

    pub fn row(&self, run_id: &str) -> Option<&TelemetryRow> {
        self.rows.get(run_id)
    }

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
        let billed_tokens = usage.input + usage.cache_read + usage.cache_write;
        ThreadMetrics {
            cost: has_cost.then_some(cost_total),
            cache_hit_rate: (billed_tokens > 0).then(|| usage.cache_hit_rate()),
            wall_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{Event, ProviderEvent, ToolEvent};

    fn request_started(run_id: Option<&str>) -> Event {
        Event::new(ProviderEvent::RequestStarted {
            request_id: "request-1".into(),
            provider: "provider-a".into(),
            profile: None,
            protocol: "protocol-a".into(),
            model: "model-a".into(),
            streaming: true,
            run_id: run_id.map(str::to_owned),
        })
    }

    fn request_completed(run_id: Option<&str>, input: u64, output: u64) -> Event {
        Event::new(ProviderEvent::RequestCompleted {
            request_id: "request-1".into(),
            provider: "provider-a".into(),
            profile: None,
            protocol: "protocol-a".into(),
            model: "model-a".into(),
            streaming: true,
            duration_ms: 10,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            finish_reason: "stop".into(),
            run_id: run_id.map(str::to_owned),
        })
    }

    #[test]
    fn provider_and_model_come_from_request_started() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&request_started(Some("run-1")));

        let row = overlay.row("run-1").expect("telemetry row");
        assert_eq!(row.provider.as_deref(), Some("provider-a"));
        assert_eq!(row.model.as_deref(), Some("model-a"));
        assert_eq!(row.requests, 1);
    }

    #[test]
    fn tokens_accumulate_from_request_completed_only() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&request_completed(Some("run-1"), 10, 20));
        overlay.apply_event(&request_completed(Some("run-1"), 5, 7));

        assert_eq!(
            overlay.row("run-1").expect("telemetry row").usage,
            TokenUsage {
                input: 15,
                output: 27,
                cache_read: 6,
                cache_write: 8,
            }
        );
    }

    #[test]
    fn current_tool_set_and_cleared() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&Event::new(ToolEvent::ToolStarted {
            input: None,
            tool_name: "read".into(),
            call_id: "call-1".into(),
            run_id: Some("run-1".into()),
        }));
        assert_eq!(
            overlay
                .row("run-1")
                .expect("telemetry row")
                .current_tool
                .as_deref(),
            Some("read")
        );

        overlay.apply_event(&Event::new(ToolEvent::ToolCompleted {
            output: None,
            tool_name: "read".into(),
            call_id: "call-1".into(),
            is_error: false,
            detail: None,
            run_id: Some("run-1".into()),
        }));
        assert!(
            overlay
                .row("run-1")
                .expect("telemetry row")
                .current_tool
                .is_none()
        );
    }

    fn agent_run_started(run_id: &str) -> Event {
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: run_id.into(),
            parent_run_id: None,
            agent_name: "agent".into(),
            role: "worker".into(),
        })
    }

    fn agent_run_finished(run_id: &str, to: AgentRunPhase) -> Event {
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: run_id.into(),
            from: AgentRunPhase::Running,
            to,
            reason: None,
        })
    }

    #[test]
    fn wall_time_accumulates_between_run_start_and_terminal_state() {
        let mut overlay = TelemetryOverlay::new();
        let start = Instant::now();
        overlay.apply_event_at(&agent_run_started("run-1"), start);
        overlay.apply_event_at(
            &agent_run_finished("run-1", AgentRunPhase::Done),
            start + Duration::from_secs(90),
        );

        let metrics = overlay.thread_metrics_at(&["run-1".to_owned()], start + Duration::from_secs(120));
        assert_eq!(metrics.wall_time, Duration::from_secs(90));
    }

    #[test]
    fn wall_time_includes_in_flight_runs() {
        let mut overlay = TelemetryOverlay::new();
        let start = Instant::now();
        overlay.apply_event_at(&agent_run_started("run-1"), start);
        overlay.apply_event_at(
            &agent_run_finished("run-1", AgentRunPhase::Error),
            start + Duration::from_secs(30),
        );
        overlay.apply_event_at(&agent_run_started("run-2"), start + Duration::from_secs(40));

        let metrics = overlay.thread_metrics_at(
            &["run-1".to_owned(), "run-2".to_owned()],
            start + Duration::from_secs(100),
        );
        assert_eq!(metrics.wall_time, Duration::from_secs(90));
    }

    #[test]
    fn thread_metrics_aggregates_cache_hit_rate_across_runs() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&request_completed(Some("run-1"), 100, 10));
        overlay.apply_event(&request_completed(Some("run-2"), 100, 10));

        let metrics = overlay.thread_metrics(&["run-1".to_owned(), "run-2".to_owned()]);
        let rate = metrics.cache_hit_rate.expect("cache hit rate");
        assert!((rate - (6.0 / 214.0 * 100.0)).abs() < 0.01);
        assert!(metrics.cost.is_none());
    }

    #[test]
    fn thread_metrics_empty_for_unknown_runs() {
        let overlay = TelemetryOverlay::new();
        let metrics = overlay.thread_metrics(&["missing".to_owned()]);
        assert_eq!(metrics, ThreadMetrics::default());
    }
}
