//! Cache ratios use provider token counts. Regression comparisons require an
//! unchanged, recently observed wire prefix; request size is not cache residency.
use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use event_bus::{
    CACHE_RETENTION_WARNING_THRESHOLD, CacheBaselineMissing, CacheComparison, DiagnosticEvent,
    DiagnosticSeverity, Event, EventBus, ProviderEvent,
};
use tokio::time::Instant;

use super::AttemptObserver;
use crate::message::Usage;

#[path = "cache_prefix.rs"]
mod prefix;
use prefix::{RequestPrefix, WireInput};

const MAX_SCOPES: usize = 1024;
// A conservative comparison window, not a guarantee of server cache residency.
const MAX_BASELINE_AGE: Duration = Duration::from_secs(5 * 60);
static RECENT_REQUESTS: LazyLock<Mutex<VecDeque<CachedRequest>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

#[derive(Clone, PartialEq, Eq)]
struct CacheScope {
    run: String,
    provider: String,
    profile: Option<String>,
    protocol: &'static str,
    model: String,
}

#[derive(Clone)]
struct CachedRequest {
    bus: Weak<EventBus>,
    scope: CacheScope,
    prefix: RequestPrefix,
    request_id: String,
    cached_tokens: u64,
    started_at: Instant,
}

pub(super) struct CacheObservation {
    prefix: RequestPrefix,
    baseline: Result<CachedRequest, CacheBaselineMissing>,
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| {
        Duration::from_secs(numerator).as_secs_f64()
            / Duration::from_secs(denominator).as_secs_f64()
    })
}

impl AttemptObserver {
    fn cache_scope(&self) -> Option<CacheScope> {
        Some(CacheScope {
            run: self.observation.as_ref()?.run_id.clone(),
            provider: self.provider.clone(),
            profile: self.profile.clone(),
            protocol: self.protocol,
            model: self.model.clone(),
        })
    }

    /// Snapshot before sending, so concurrent cold requests cannot warm each
    /// other retroactively. Persist only fixed-size hashes, never prompt text.
    pub(super) fn observe_cache_request(
        &self,
        request: &impl serde::Serialize,
    ) -> Option<CacheObservation> {
        let wire = WireInput::new(request, self.protocol)?;
        let mut expired = false;
        let previous = self
            .bus
            .as_ref()
            .zip(self.cache_scope())
            .and_then(|(bus, scope)| {
                let mut recent = RECENT_REQUESTS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let in_scope = |entry: &CachedRequest| {
                    entry.bus.ptr_eq(&Arc::downgrade(bus)) && entry.scope == scope
                };
                expired = recent
                    .iter()
                    .any(|entry| in_scope(entry) && entry.started_at.elapsed() >= MAX_BASELINE_AGE);
                recent.retain(|entry| {
                    entry.bus.strong_count() > 0 && entry.started_at.elapsed() < MAX_BASELINE_AGE
                });
                recent.iter().find(|entry| in_scope(entry)).cloned()
            });
        let baseline = match previous {
            None if expired => Err(CacheBaselineMissing::Expired),
            None => Err(CacheBaselineMissing::NoPreviousRequest),
            Some(entry) if !wire.extends(&entry.prefix) => Err(CacheBaselineMissing::PrefixChanged),
            Some(entry) if entry.cached_tokens == 0 => Err(CacheBaselineMissing::PreviousUncached),
            Some(entry) => Ok(entry),
        };
        Some(CacheObservation {
            prefix: wire.prefix()?,
            baseline,
        })
    }

    pub(super) fn record_cache(&self, usage: &Usage) {
        let valid_usage = usage
            .cache_read_tokens
            .checked_add(usage.cache_write_tokens)
            .filter(|cached| *cached <= usage.input_tokens);
        let hit_ratio =
            valid_usage.and_then(|_| ratio(usage.cache_read_tokens, usage.input_tokens));
        let baseline = self
            .cache
            .as_ref()
            .and_then(|cache| cache.baseline.as_ref().ok());
        let previous_cache_tokens = baseline.map(|entry| entry.cached_tokens);
        let retention_ratio = valid_usage.and_then(|_| {
            previous_cache_tokens.and_then(|tokens| ratio(usage.cache_read_tokens, tokens))
        });
        tracing::info!(
            request_id = %self.request_id,
            run_id = self.observation.as_ref().map(|context| context.run_id.as_str()),
            provider = %self.provider,
            profile = self.profile.as_deref(),
            protocol = self.protocol,
            model = %self.model,
            input_tokens = usage.input_tokens,
            cache_read_tokens = usage.cache_read_tokens,
            cache_hit_ratio = hit_ratio,
            previous_request_id = baseline.map(|entry| entry.request_id.as_str()),
            previous_cache_tokens,
            cache_retention_ratio = retention_ratio,
            cache_comparison = "unchanged_wire_prefix",
            "prompt cache request completed"
        );
        let Some((bus, scope)) = self.bus.as_ref().zip(self.cache_scope()) else {
            return;
        };
        let Some(cache) = &self.cache else {
            return;
        };
        let comparison = match (valid_usage, &cache.baseline) {
            (None, _) => CacheComparison::NoBaseline {
                reason: CacheBaselineMissing::InvalidUsage,
            },
            (Some(_), Ok(entry)) => CacheComparison::Compared {
                previous_request_id: entry.request_id.clone(),
                previous_cache_tokens: entry.cached_tokens,
            },
            (Some(_), Err(reason)) => CacheComparison::NoBaseline { reason: *reason },
        };
        self.emit(ProviderEvent::CacheReuseObserved {
            request_id: self.request_id.clone(),
            cache_read_tokens: usage.cache_read_tokens,
            comparison,
            run_id: Some(scope.run.clone()),
        });
        if let (Some(baseline), Some(retention), Some(hit)) = (baseline, retention_ratio, hit_ratio)
            && retention < CACHE_RETENTION_WARNING_THRESHOLD
        {
            bus.emit(Event::new(DiagnosticEvent {
                source: "providers.cache".into(),
                severity: DiagnosticSeverity::Warning,
                code: "CacheRegression".into(),
                detail: format!(
                    "provider={} profile={:?} protocol={} model={} input_tokens={} cache_read_tokens={} cache_hit_ratio={} previous_request_id={} previous_cache_tokens={} cache_retention_ratio={} threshold={CACHE_RETENTION_WARNING_THRESHOLD}",
                    self.provider, self.profile, self.protocol, self.model,
                    usage.input_tokens, usage.cache_read_tokens, hit,
                    baseline.request_id, baseline.cached_tokens, retention,
                ),
                run_id: Some(scope.run.clone()),
                thread_id: None,
                call_id: Some(self.request_id.clone()),
            }));
        }
        let mut recent = RECENT_REQUESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A late response from an earlier concurrent attempt must not replace
        // the newest request's baseline with an older branch of the conversation.
        if recent.iter().any(|entry| {
            entry.bus.ptr_eq(&Arc::downgrade(bus))
                && entry.scope == scope
                && entry.started_at > self.started_at
        }) {
            return;
        }
        recent.retain(|entry| {
            entry.bus.strong_count() > 0
                && !(entry.bus.ptr_eq(&Arc::downgrade(bus)) && entry.scope == scope)
        });
        if recent.len() >= MAX_SCOPES {
            recent.pop_front();
        }
        recent.push_back(CachedRequest {
            bus: Arc::downgrade(bus),
            scope,
            prefix: cache.prefix.clone(),
            request_id: self.request_id.clone(),
            cached_tokens: valid_usage.unwrap_or_default(),
            started_at: self.started_at,
        });
    }
}
