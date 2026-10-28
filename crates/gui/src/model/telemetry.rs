//! run ID が明示された観測イベントだけを集約する telemetry overlay。
//!
//! `UsageEvent::Usage` は `run_id` を持たないため常に無視する。表示用 token 数は
//! `ProviderEvent::RequestCompleted` の値だけを使い、未知の provider/model は推測せず
//! `None` のまま保持する。

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use event_bus::{
    AgentRunPhase, Event, EventKind, LifecycleEvent, MessageEvent, ProviderEvent, RequestPurpose,
    ToolEvent,
};
use recorder::pricing::TokenUsage;

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
pub use thread_metrics::latency_label;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryRow {
    pub provider: Option<String>,
    /// The routing provider profile the user named, when the request had one.
    pub profile: Option<String>,
    pub model: Option<String>,
    pub current_tool: Option<String>,
    pub activity: Option<event_bus::RunActivity>,
    pub context_composition: Option<event_bus::ContextComposition>,
    pub last_checkpoint: Option<std::time::SystemTime>,
    pub checkpoint_failure: Option<String>,
    pub usage: TokenUsage,
    conversation_usage: TokenUsage,
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
    /// Completed output tokens and generation time that throughput averages rate.
    rated_output_tokens: u64,
    rated_duration: Duration,
    pub output_tokens: u64,
    streamed_chars: u64,
    /// Share of `streamed_chars` that arrived as reasoning text.
    reasoning_chars: u64,
    /// The provider streams only a summary of its reasoning, so the reasoning
    /// tokens it bills never arrive and are not rated as throughput.
    summarized_reasoning: bool,
    /// Reported reasoning share of the completed request's `output_tokens`.
    reasoning_tokens: Option<u64>,
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
        let (tokens, elapsed) = self.rated_output_at(now)?;
        if tokens == 0 {
            return None;
        }
        // Duration converts the u64 count without a lossy integer narrowing cast.
        (!elapsed.is_zero())
            .then(|| Duration::from_secs(tokens).as_secs_f64() / elapsed.as_secs_f64())
    }

    /// Output tokens and the time spent generating them. Summarized reasoning
    /// is excluded from both: its tokens and the time before the first visible
    /// output, which `ttft` measures without reasoning deltas.
    fn rated_output_at(&self, now: Instant) -> Option<(u64, Duration)> {
        let elapsed = self.elapsed_at(now)?;
        if !self.summarized_reasoning {
            return Some((self.output_tokens, elapsed));
        }
        let tokens = match self.reasoning_tokens {
            Some(reasoning) => self.output_tokens.saturating_sub(reasoning),
            None => (self.streamed_chars - self.reasoning_chars).div_ceil(4),
        };
        let hidden = Duration::from_millis(self.ttft_ms?);
        Some((tokens, elapsed.checked_sub(hidden)?))
    }

    pub fn average_ttft_ms(&self) -> Option<u64> {
        (self.ttft_count > 0).then(|| self.ttft_sum_ms / self.ttft_count)
    }

    pub fn latest_ttft_ms(&self) -> Option<u64> {
        self.ttft_ms
    }

    /// The user's routing profile name, falling back to the provider kind.
    pub fn provider_label(&self) -> Option<&str> {
        self.profile.as_deref().or(self.provider.as_deref())
    }
}

/// Responses API streams reasoning summaries in place of reasoning text.
fn protocol_summarizes_reasoning(protocol: &str) -> bool {
    matches!(protocol, "openai-codex-responses" | "openai-responses")
}

/// Side requests share the run's bill, not its live conversation metrics.
fn is_side_request(purpose: Option<RequestPurpose>) -> bool {
    matches!(
        purpose,
        Some(RequestPurpose::EscalationReview | RequestPurpose::ImageGeneration)
    )
}

#[derive(Debug, Default)]
pub struct TelemetryOverlay {
    pub quota: quota::QuotaState,
    pub kimi_quota: super::kimi_quota::KimiQuotaState,
    pub subscription_quota: super::subscription_quota::SubscriptionQuotaState,
    rows: BTreeMap<String, TelemetryRow>,
    billed: BTreeMap<String, Vec<pricing::BilledRequest>>,
    costs: BTreeMap<String, f64>,
    /// Runs whose cost leaves out requests without a known price.
    partially_priced: BTreeSet<String>,
    /// Settings and catalog that `schedules` and `context_windows` were resolved from.
    pricing_source: Option<pricing::PricingSource>,
    schedules: BTreeMap<pricing::ModelKey, Option<config::types::provider::PriceSchedule>>,
    context_windows: BTreeMap<pricing::ModelKey, Option<u64>>,
    /// Runs billed since their cost was last summed.
    stale_costs: BTreeSet<String>,
    /// Rows whose latest request changed since their context window was resolved.
    stale_context: BTreeSet<String>,
    active_running_start: BTreeMap<String, Instant>,
    /// Finished Running spans per run, oldest first. Kept as spans rather than a
    /// total so concurrent runs can be merged into real elapsed time.
    running_spans: BTreeMap<String, Vec<(Instant, Instant)>>,
    context_order: u64,
    side_requests: BTreeSet<String>,
}

/// Thread totals plus the conversation roots' latest and average request measurements.
/// Costs and running time include owned children; cache and model performance do not.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ThreadMetrics {
    /// Sum across the conversation and every subagent owned by this thread.
    pub cost: Option<f64>,
    /// `cost` leaves out requests without a known price.
    pub cost_partial: bool,
    /// Only the conversation roots, excluding launched subagents.
    pub conversation_cost: Option<f64>,
    /// `conversation_cost` leaves out requests without a known price.
    pub conversation_cost_partial: bool,
    /// Billed `cache_read / input` of the latest request, kept for tooltips.
    pub cache_hit_rate: Option<f64>,
    /// Billed `cache_read / input` across the conversation, kept for tooltips.
    pub average_cache_hit_rate: Option<f64>,
    /// Retention of the conversation roots, the displayed cache health metric.
    pub cache_reuse: CacheReuseSummary,
    /// Elapsed time during which at least one run of the thread was Running:
    /// the union of the runs' Running spans, so parallel subagents overlap.
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
                request_id,
                purpose,
                run_id: Some(_),
                ..
            }) if is_side_request(*purpose) => {
                self.side_requests.insert(request_id.clone());
            }
            EventKind::Provider(
                ProviderEvent::CacheReuseObserved { request_id, .. }
                | ProviderEvent::FirstTokenObserved { request_id, .. },
            ) if self.side_requests.contains(request_id) => {}
            EventKind::Provider(ProviderEvent::RequestStarted {
                provider,
                profile,
                protocol,
                model,
                run_id: Some(run_id),
                ..
            }) => {
                self.context_order = self.context_order.saturating_add(1);
                let row = self.rows.entry(run_id.clone()).or_default();
                row.context_order = self.context_order;
                row.in_flight = true;
                row.provider = Some(provider.clone());
                row.profile.clone_from(profile);
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
                        self.stale_context.insert(run_id.clone());
                    }
                }
                row.requests = row.requests.saturating_add(1);
                row.request_started_at = Some(now);
                row.request_duration = None;
                row.ttft_ms = None;
                row.output_tokens = 0;
                row.streamed_chars = 0;
                row.reasoning_chars = 0;
                row.reasoning_tokens = None;
                // Completion confirms this from the reported reasoning tokens.
                row.summarized_reasoning = protocol_summarizes_reasoning(protocol);
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
                message @ (MessageEvent::MessageDelta {
                    delta,
                    run_id: Some(run_id),
                }
                | MessageEvent::ReasoningDelta {
                    delta,
                    run_id: Some(run_id),
                }),
            ) => {
                if let Some(row) = self.rows.get_mut(run_id)
                    && row.request_started_at.is_some()
                    && row.request_duration.is_none()
                {
                    // Deltas carry no token count: estimate across chunks, never bill this value.
                    let chars = delta
                        .chars()
                        .fold(0_u64, |count, _| count.saturating_add(1));
                    row.streamed_chars = row.streamed_chars.saturating_add(chars);
                    if matches!(message, MessageEvent::ReasoningDelta { .. }) {
                        row.reasoning_chars = row.reasoning_chars.saturating_add(chars);
                    }
                    row.output_tokens = row.streamed_chars.div_ceil(4);
                }
            }
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
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
                reasoning_tokens,
                service_tier,
                purpose,
                ..
            }) => {
                self.billed
                    .entry(run_id.clone())
                    .or_default()
                    .push(pricing::BilledRequest {
                        key: pricing::ModelKey {
                            provider: provider.clone(),
                            profile: profile.clone(),
                            model: model.clone(),
                        },
                        speed: recorder::pricing::speed(*service_tier),
                        usage: TokenUsage {
                            input: *input_tokens,
                            output: *output_tokens,
                            cache_read: *cache_read_tokens,
                            cache_write: *cache_write_tokens,
                        },
                    });
                self.stale_costs.insert(run_id.clone());
                let row = self.rows.entry(run_id.clone()).or_default();
                row.usage.input = row.usage.input.saturating_add(*input_tokens);
                row.usage.output = row.usage.output.saturating_add(*output_tokens);
                row.usage.cache_read = row.usage.cache_read.saturating_add(*cache_read_tokens);
                row.usage.cache_write = row.usage.cache_write.saturating_add(*cache_write_tokens);
                if self.side_requests.remove(request_id) || is_side_request(*purpose) {
                    return;
                }
                row.conversation_usage.input =
                    row.conversation_usage.input.saturating_add(*input_tokens);
                row.conversation_usage.cache_read = row
                    .conversation_usage
                    .cache_read
                    .saturating_add(*cache_read_tokens);
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
                self.stale_context.insert(run_id.clone());
                row.in_flight = false;
                row.last_finish_reason = Some(finish_reason.clone());
                row.output_tokens = *output_tokens;
                let duration = Duration::from_millis(*duration_ms);
                row.request_duration = Some(duration);
                let reasoning = reasoning_tokens.unwrap_or_default();
                // A provider without a summary protocol may still hide reasoning:
                // fewer visible characters than reasoning tokens cannot be its text.
                row.summarized_reasoning =
                    reasoning > 0 && (row.summarized_reasoning || row.reasoning_chars < reasoning);
                row.reasoning_tokens = row.summarized_reasoning.then_some(reasoning);
                if let Some((tokens, rated)) = row.rated_output_at(now) {
                    row.rated_output_tokens = row.rated_output_tokens.saturating_add(tokens);
                    row.rated_duration = row.rated_duration.saturating_add(rated);
                }
            }
            EventKind::Provider(ProviderEvent::RequestFailed {
                request_id,
                purpose,
                ..
            }) if self.side_requests.remove(request_id) || is_side_request(*purpose) => {}
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
                let row = self.rows.entry(run_id.clone()).or_default();
                row.ttft_ms = None;
                row.parent_run_id.clone_from(parent_run_id);
                row.conversation_root |= agent_name.starts_with("chat:");
                row.ttft_sum_ms = 0;
                row.ttft_count = 0;
            }
            EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                new_run_id: run_id,
                ..
            }) => {
                self.rows
                    .entry(run_id.clone())
                    .or_default()
                    .conversation_root = true;
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
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
                            self.running_spans
                                .entry(run_id.clone())
                                .or_default()
                                .push((start, now.max(start)));
                        }
                        // An ended run executes no tool, even if a call of it
                        // never reported a result.
                        if *to != AgentRunPhase::Waiting
                            && let Some(row) = self.rows.get_mut(run_id)
                        {
                            row.current_tool = None;
                            row.activity = None;
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
            | EventKind::Ownership(_) => {}
        }
    }

    /// Persisted progress is history, never evidence of a live request after restart.
    pub fn finish_history(&mut self) {
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
