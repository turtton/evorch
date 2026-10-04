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

#[path = "cache_reuse.rs"]
mod cache_reuse;
pub use cache_reuse::{CacheReuseSummary, RequestReuse};

#[path = "context_pressure.rs"]
mod context_pressure;

#[path = "thread_metrics.rs"]
mod thread_metrics;

#[path = "workspace_wait.rs"]
mod workspace_wait;
pub use workspace_wait::WorkspaceWaitEntry;

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
    pub activity: Option<event_bus::RunActivity>,
    pub context_composition: Option<event_bus::ContextComposition>,
    pub last_checkpoint: Option<std::time::SystemTime>,
    pub checkpoint_failure: Option<String>,
    pub usage: TokenUsage,
    pub cache_reuse: CacheReuseSummary,
    pending_reuse: Option<RequestReuse>,
    latest_context: Option<context_pressure::RequestContext>,
    in_flight: bool,
    context_order: u64,
    parent_run_id: Option<String>,
    conversation_root: bool,
    context_window: Option<u64>,
    pub requests: u32,
    pub last_finish_reason: Option<String>,
    pub request_started_at: Option<Instant>,
    ttft_ms: Option<u64>,
    ttft_sum_ms: u64,
    ttft_count: u64,
    pub request_duration: Option<Duration>,
    completed_request_duration: Duration,
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
        // Text/reasoning deltas are estimates; tool arguments have no GUI delta.
        // Until any output is observable, displaying 0 tok/s invents a sample.
        if self.output_tokens == 0 {
            return None;
        }
        let elapsed = self.elapsed_at(now)?;
        // Duration converts the u64 count without a lossy integer narrowing cast.
        (!elapsed.is_zero())
            .then(|| Duration::from_secs(self.output_tokens).as_secs_f64() / elapsed.as_secs_f64())
    }

    pub fn average_ttft_ms(&self) -> Option<u64> {
        (self.ttft_count > 0).then(|| self.ttft_sum_ms / self.ttft_count)
    }

    pub fn latest_ttft_ms(&self) -> Option<u64> {
        self.ttft_ms
    }
}

#[derive(Debug, Default)]
pub struct TelemetryOverlay {
    pub quota: quota::QuotaState,
    pub kimi_quota: super::kimi_quota::KimiQuotaState,
    rows: BTreeMap<String, TelemetryRow>,
    billed: BTreeMap<String, BTreeMap<pricing::ModelKey, TokenUsage>>,
    costs: BTreeMap<String, f64>,
    active_running_start: BTreeMap<String, Instant>,
    accumulated_running: BTreeMap<String, Duration>,
    context_order: u64,
    workspace_waits: BTreeMap<(String, String), WorkspaceWaitEntry>,
}

/// Thread totals plus the conversation roots' latest and average request measurements.
/// Costs and wall time include owned children; cache and model performance do not.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ThreadMetrics {
    /// Sum across the conversation and every subagent owned by this thread.
    pub cost: Option<f64>,
    /// Only the conversation roots, excluding launched subagents.
    pub conversation_cost: Option<f64>,
    /// Billed `cache_read / input` of the latest request, kept for tooltips.
    pub cache_hit_rate: Option<f64>,
    /// Billed `cache_read / input` across the conversation, kept for tooltips.
    pub average_cache_hit_rate: Option<f64>,
    /// Retention of the conversation roots, the displayed cache health metric.
    pub cache_reuse: CacheReuseSummary,
    pub wall_time: Duration,
    pub context_pressure: Option<u128>,
    pub context_used_tokens: Option<u128>,
    pub ttft: Option<Duration>,
    /// Mean of the first-token observations across the conversation requests.
    pub average_ttft: Option<Duration>,
    pub tok_s: Option<f64>,
    /// Completed output tokens divided by completed provider request duration.
    pub average_tok_s: Option<f64>,
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
            EventKind::Lifecycle(LifecycleEvent::WorkspaceWaitChanged {
                run_id,
                call_id,
                waiting,
            }) => self.update_workspace_wait(run_id, call_id, waiting.as_ref(), now),
            EventKind::Lifecycle(LifecycleEvent::RunProgress {
                run_id,
                activity,
                context,
            }) => {
                let row = self.rows.entry(run_id.clone()).or_default();
                row.activity = Some(*activity);
                if let Some(context) = context {
                    row.context_composition = Some(context.clone());
                }
            }
            EventKind::Diagnostic(d)
                if d.code == "ContextCheckpointSaved" || d.code == "ContextSnapshotFailed" =>
            {
                if let Some(run) = &d.run_id {
                    let row = self.rows.entry(run.clone()).or_default();
                    if d.code == "ContextCheckpointSaved" {
                        row.last_checkpoint = Some(event.meta.wall_clock);
                        row.checkpoint_failure = None;
                    } else {
                        row.checkpoint_failure = Some(d.detail.clone());
                    }
                }
            }
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
                row.ttft_ms = None;
                row.output_tokens = 0;
                row.streamed_chars = 0;
                row.pending_reuse = None;
            }
            EventKind::Provider(ProviderEvent::CacheReuseObserved {
                cache_read_tokens,
                comparison,
                run_id: Some(run_id),
                ..
            }) => {
                self.rows.entry(run_id.clone()).or_default().pending_reuse =
                    Some(RequestReuse::observed(*cache_read_tokens, comparison));
            }
            EventKind::Provider(ProviderEvent::FirstTokenObserved {
                ttft_ms,
                run_id: Some(run_id),
                ..
            }) => {
                let row = self.rows.entry(run_id.clone()).or_default();
                row.ttft_sum_ms = row.ttft_sum_ms.saturating_add(*ttft_ms);
                row.ttft_count = row.ttft_count.saturating_add(1);
                row.ttft_ms = Some(*ttft_ms);
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
                let reuse = row.pending_reuse.take().unwrap_or(RequestReuse::Unobserved);
                row.cache_reuse.complete(reuse);
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
                let duration = Duration::from_millis(*duration_ms);
                row.request_duration = Some(duration);
                row.completed_request_duration =
                    row.completed_request_duration.saturating_add(duration);
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
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                agent_name,
                ..
            }) => {
                self.clear_workspace_waits(run_id);
                let row = self.rows.entry(run_id.clone()).or_default();
                row.ttft_ms = None;
                row.parent_run_id.clone_from(parent_run_id);
                row.conversation_root |= agent_name.starts_with("chat:");
                row.ttft_sum_ms = 0;
                row.ttft_count = 0;
                self.accumulated_running.entry(run_id.clone()).or_default();
            }
            EventKind::Orchestrator(event_bus::OrchestratorEvent::GoalCreated {
                root_run_id: run_id,
                ..
            })
            | EventKind::Orchestrator(event_bus::OrchestratorEvent::ContinuationDispatched {
                new_run_id: run_id,
                ..
            })
            | EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                new_run_id: run_id,
                ..
            }) => {
                self.rows
                    .entry(run_id.clone())
                    .or_default()
                    .conversation_root = true;
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                if matches!(
                    to,
                    AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error
                ) {
                    self.clear_workspace_waits(run_id);
                }
                match to {
                    AgentRunPhase::Running => {
                        self.active_running_start
                            .entry(run_id.clone())
                            .or_insert(now);
                    }
                    AgentRunPhase::Stopped
                    | AgentRunPhase::Waiting
                    | AgentRunPhase::Done
                    | AgentRunPhase::Error => {
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

    /// Persisted progress is history, never evidence of a live request after restart.
    pub fn finish_history(&mut self) {
        self.workspace_waits.clear();
        self.active_running_start.clear();
        for row in self.rows.values_mut() {
            row.activity = None;
            row.current_tool = None;
            row.in_flight = false;
            row.request_started_at = None;
        }
    }

    pub fn row(&self, run_id: &str) -> Option<&TelemetryRow> {
        self.rows.get(run_id)
    }

    /// Every run observed this session, in run ID order.
    pub fn run_ids(&self) -> impl Iterator<Item = &str> {
        self.rows.keys().map(String::as_str)
    }
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "telemetry_request_tests.rs"]
mod request_tests;
