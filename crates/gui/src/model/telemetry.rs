//! run ID が明示された観測イベントだけを集約する telemetry overlay。
//!
//! `UsageEvent::Usage` は `run_id` を持たないため常に無視する。表示用 token 数は
//! `ProviderEvent::RequestCompleted` の値だけを使い、未知の provider/model は推測せず
//! `None` のまま保持する。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use event_bus::{
    AgentRunPhase, Event, EventKind, LifecycleEvent, MessageEvent, ProviderEvent, ToolEvent,
};

#[path = "pricing.rs"]
pub mod pricing;

#[path = "quota.rs"]
pub mod quota;

#[path = "context_pressure.rs"]
mod context_pressure;

#[path = "thread_metrics.rs"]
mod thread_metrics;

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
    latest_context: Option<context_pressure::RequestContext>,
    in_flight: bool,
    context_order: u64,
    context_window: Option<u64>,
    pub requests: u32,
    pub last_finish_reason: Option<String>,
    pub request_started_at: Option<Instant>,
    ttft_ms: Option<u64>,
    ttft_sum_ms: u64,
    ttft_count: u64,
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

    pub fn average_ttft_ms(&self) -> Option<u64> {
        self.ttft_ms
    }
}

#[derive(Debug, Default)]
pub struct TelemetryOverlay {
    pub quota: quota::QuotaState,
    rows: BTreeMap<String, TelemetryRow>,
    billed: BTreeMap<String, BTreeMap<pricing::ModelKey, TokenUsage>>,
    costs: BTreeMap<String, f64>,
    active_running_start: BTreeMap<String, Instant>,
    accumulated_running: BTreeMap<String, Duration>,
    context_order: u64,
}

/// スレッドに紐づく全 run の累計メトリクス。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ThreadMetrics {
    pub cost: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub wall_time: Duration,
    pub context_pressure: Option<u128>,
    pub ttft: Option<Duration>,
    pub tok_s: Option<f64>,
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
                profile,
                model,
                run_id: Some(run_id),
                ..
            }) => {
                self.context_order = self.context_order.saturating_add(1);
                let row = self.rows.entry(run_id.clone()).or_default();
                row.context_order = self.context_order;
                row.in_flight = true;
                row.provider = Some(provider.clone());
                row.model = Some(model.clone());
                if let Some(context) = &mut row.latest_context {
                    let key = pricing::ModelKey {
                        provider: provider.clone(),
                        profile: profile.clone(),
                        model: model.clone(),
                    };
                    if context.key != key {
                        context.key = key;
                        row.context_window = None;
                    }
                }
                row.requests = row.requests.saturating_add(1);
                row.request_started_at = Some(now);
                row.request_duration = None;
                row.output_tokens = 0;
                row.streamed_chars = 0;
            }
            EventKind::Provider(ProviderEvent::FirstTokenObserved {
                ttft_ms,
                run_id: Some(run_id),
                ..
            }) => {
                let row = self.rows.entry(run_id.clone()).or_default();
                row.ttft_sum_ms = row.ttft_sum_ms.saturating_add(*ttft_ms);
                row.ttft_count = row.ttft_count.saturating_add(1);
                row.ttft_ms = Some(row.ttft_sum_ms / row.ttft_count);
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
                self.context_order = self.context_order.saturating_add(1);
                row.context_order = self.context_order;
                row.latest_context = Some(context_pressure::RequestContext {
                    key: pricing::ModelKey {
                        provider: provider.clone(),
                        profile: profile.clone(),
                        model: model.clone(),
                    },
                    usage: TokenUsage {
                        input: *input_tokens,
                        output: *output_tokens,
                        cache_read: *cache_read_tokens,
                        cache_write: *cache_write_tokens,
                    },
                });
                row.context_window = None;
                row.in_flight = false;
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
                row.in_flight = false;
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
                let row = self.rows.entry(run_id.clone()).or_default();
                row.ttft_ms = None;
                row.ttft_sum_ms = 0;
                row.ttft_count = 0;
                self.accumulated_running.entry(run_id.clone()).or_default();
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                match to {
                    AgentRunPhase::Running => {
                        self.active_running_start
                            .entry(run_id.clone())
                            .or_insert(now);
                    }
                    AgentRunPhase::Waiting | AgentRunPhase::Done | AgentRunPhase::Error => {
                        if let Some(start) = self.active_running_start.remove(run_id) {
                            *self.accumulated_running.entry(run_id.clone()).or_default() +=
                                now.saturating_duration_since(start);
                        }
                    }
                    AgentRunPhase::Pending => {}
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
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;
