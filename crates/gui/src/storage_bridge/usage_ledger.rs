//! Builds usage ledger rows from provider attempt terminal events.

use std::collections::HashMap;

use event_bus::{Event, EventKind, LifecycleEvent, ProviderEvent, ProviderFailureKind};
use storage::usage::{UsageRequestRecord, UsageStatus};

use crate::model::telemetry::TokenUsage;
use crate::model::telemetry::pricing::SharedUsagePricing;

/// Tracks the run tree and first-token latency the terminal event lacks, and
/// prices each completed attempt at the rates in force when it is recorded.
pub(crate) struct UsageRecorder {
    pricing: SharedUsagePricing,
    runs: HashMap<String, RunInfo>,
    first_token_ms: HashMap<String, u64>,
}

struct RunInfo {
    parent_run_id: Option<String>,
    role: String,
}

impl UsageRecorder {
    pub(crate) fn new(pricing: SharedUsagePricing) -> Self {
        Self {
            pricing,
            runs: HashMap::new(),
            first_token_ms: HashMap::new(),
        }
    }

    /// Returns a ledger row for a terminal attempt event, otherwise records context.
    pub(crate) fn observe(&mut self, event: &Event) -> Option<UsageRequestRecord> {
        let (request_id, provider, profile, model, run_id, purpose) = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                role,
                ..
            }) => {
                self.runs.insert(
                    run_id.clone(),
                    RunInfo {
                        parent_run_id: parent_run_id.clone(),
                        role: role.clone(),
                    },
                );
                return None;
            }
            EventKind::Provider(ProviderEvent::FirstTokenObserved {
                request_id,
                ttft_ms,
                ..
            }) => {
                self.first_token_ms.insert(request_id.clone(), *ttft_ms);
                return None;
            }
            EventKind::Provider(
                ProviderEvent::RequestCompleted {
                    request_id,
                    provider,
                    profile,
                    model,
                    run_id,
                    purpose,
                    ..
                }
                | ProviderEvent::RequestFailed {
                    request_id,
                    provider,
                    profile,
                    model,
                    run_id,
                    purpose,
                    ..
                },
            ) => (request_id, provider, profile, model, run_id, purpose),
            _ => return None,
        };
        let run = run_id.as_deref().and_then(|run| self.runs.get(run));
        let mut record = UsageRequestRecord {
            request_id: request_id.clone(),
            at_ns: storage::system_time_to_ns(event.meta.wall_clock).ok()?,
            provider: provider.clone(),
            profile: profile.clone(),
            model: model.clone(),
            run_id: run_id.clone(),
            parent_run_id: run.and_then(|run| run.parent_run_id.clone()),
            role: run.map(|run| run.role.clone()),
            purpose: purpose.map(|purpose| purpose.as_str().to_owned()),
            status: UsageStatus::Ok,
            failure: None,
            finish_reason: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            ttft_ms: self.first_token_ms.remove(request_id),
            duration_ms: 0,
            cost_usd: None,
        };
        match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                duration_ms,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                finish_reason,
                reasoning_tokens,
                ..
            }) => {
                record.duration_ms = *duration_ms;
                record.input_tokens = *input_tokens;
                record.output_tokens = *output_tokens;
                record.cache_read_tokens = *cache_read_tokens;
                record.cache_write_tokens = *cache_write_tokens;
                record.reasoning_tokens = *reasoning_tokens;
                record.finish_reason = Some(finish_reason.clone());
                let pricing = self
                    .pricing
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pricing(provider, profile.as_deref(), model);
                record.cost_usd = TokenUsage {
                    input: *input_tokens,
                    output: *output_tokens,
                    cache_read: *cache_read_tokens,
                    cache_write: *cache_write_tokens,
                }
                .estimated_cost(pricing);
            }
            EventKind::Provider(ProviderEvent::RequestFailed {
                duration_ms,
                failure,
                ..
            }) => {
                record.status = UsageStatus::Failed;
                record.duration_ms = *duration_ms;
                record.failure = Some(failure_label(*failure));
            }
            _ => {}
        }
        Some(record)
    }
}

/// Matches the labels written by the v14 backfill.
fn failure_label(failure: ProviderFailureKind) -> String {
    match failure {
        ProviderFailureKind::RateLimited => "RateLimited".into(),
        ProviderFailureKind::Http { status } => format!("Http:{status}"),
        ProviderFailureKind::Timeout => "Timeout".into(),
        ProviderFailureKind::InvalidResponse => "InvalidResponse".into(),
        ProviderFailureKind::Transport => "Transport".into(),
        ProviderFailureKind::Server => "Server".into(),
        ProviderFailureKind::Quota => "Quota".into(),
        ProviderFailureKind::Auth => "Auth".into(),
        ProviderFailureKind::Other => "Other".into(),
    }
}
