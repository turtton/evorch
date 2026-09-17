use crate::message::{ChatRequest, ContentBlock, ToolResultContent};
use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};

use super::AttemptObserver;
use crate::message::Usage;

/// Below 50% of the byte-based prefix estimate warrants investigation, not failure.
const CACHE_REGRESSION_THRESHOLD: f64 = 0.5;
/// Bound observer bookkeeping; evicted runs restart cold rather than false-alerting.
const MAX_WARM_SCOPES: usize = 1024;
static WARM_SCOPES: LazyLock<Mutex<VecDeque<WarmScope>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

#[derive(PartialEq, Eq)]
struct CacheScope {
    run: String,
    provider: String,
    profile: Option<String>,
    protocol: &'static str,
    model: String,
}

struct WarmScope {
    bus: Weak<EventBus>,
    scope: CacheScope,
}

struct CacheRegression {
    request_id: String,
    scope: CacheScope,
    expected: u64,
    actual: u64,
    ratio: f64,
}

impl From<CacheRegression> for DiagnosticEvent {
    fn from(regression: CacheRegression) -> Self {
        Self {
            source: "providers.cache".into(),
            severity: DiagnosticSeverity::Warning,
            code: "CacheRegression".into(),
            detail: format!(
                "provider={} profile={:?} protocol={} model={} expected_cacheable_tokens={} cache_read_tokens={} cache_hit_ratio={} threshold={CACHE_REGRESSION_THRESHOLD}",
                regression.scope.provider,
                regression.scope.profile,
                regression.scope.protocol,
                regression.scope.model,
                regression.expected,
                regression.actual,
                regression.ratio,
            ),
            run_id: Some(regression.scope.run),
            thread_id: None,
            call_id: Some(regression.request_id),
        }
    }
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

    /// Snapshot before sending: concurrent cold attempts cannot warm each other.
    /// Only a completed request reporting cache read/write > 0 establishes warmth
    /// in the same bus/run/provider/profile/protocol/model. Missing scope stays cold.
    pub(super) fn cache_is_warm(&self) -> bool {
        let Some((bus, scope)) = self.bus.as_ref().zip(self.cache_scope()) else {
            return false;
        };
        let mut scopes = WARM_SCOPES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        scopes.retain(|entry| entry.bus.strong_count() > 0);
        scopes
            .iter()
            .any(|entry| entry.bus.ptr_eq(&Arc::downgrade(bus)) && entry.scope == scope)
    }

    pub(super) fn record_cache(&self, usage: &Usage) {
        let Some(expected) = self.expected_cacheable_tokens else {
            return;
        };
        let ratio = u32::try_from(expected)
            .ok()
            .filter(|n| *n > 0)
            .zip(u32::try_from(usage.cache_read_tokens).ok())
            .map(|(expected, actual)| f64::from(actual) / f64::from(expected));
        tracing::info!(
            request_id = %self.request_id,
            run_id = self.observation.as_ref().map(|context| context.run_id.as_str()),
            provider = %self.provider,
            profile = self.profile.as_deref(),
            protocol = self.protocol,
            model = %self.model,
            expected_cacheable_tokens = expected,
            cache_read_tokens = usage.cache_read_tokens,
            cache_hit_ratio = ratio,
            cache_estimation = "utf8_bytes_div_4",
            "prompt cache request completed"
        );
        let Some((bus, scope)) = self.bus.as_ref().zip(self.cache_scope()) else {
            return;
        };
        if expected > 0 && (usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0) {
            let mut scopes = WARM_SCOPES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scopes.retain(|entry| {
                entry.bus.strong_count() > 0
                    && !(entry.bus.ptr_eq(&Arc::downgrade(bus)) && entry.scope == scope)
            });
            if scopes.len() >= MAX_WARM_SCOPES {
                scopes.pop_front();
            }
            scopes.push_back(WarmScope {
                bus: Arc::downgrade(bus),
                scope,
            });
        }
        if let Some(ratio) =
            ratio.filter(|ratio| self.cache_warm && *ratio < CACHE_REGRESSION_THRESHOLD)
        {
            // Rebuild only on the diagnostic path after moving the scope into bookkeeping.
            if let Some(scope) = self.cache_scope() {
                bus.emit(Event::new(DiagnosticEvent::from(CacheRegression {
                    request_id: self.request_id.clone(),
                    scope,
                    expected,
                    actual: usage.cache_read_tokens,
                    ratio,
                })));
            }
        }
    }
}

/// Approximate the reusable prefix, excluding the newest conversation message.
/// This is a byte heuristic, not a tokenizer or a prediction of cache residency.
pub(super) fn expected_cacheable_tokens(request: &ChatRequest) -> u64 {
    let newest = request
        .messages
        .iter()
        .rposition(|message| message.role != crate::message::Role::System);
    let message_bytes = request
        .messages
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != newest)
        .flat_map(|(_, message)| &message.content)
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len(),
            ContentBlock::ToolUse { id, name, input } => {
                id.len() + name.len() + input.to_string().len()
            }
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|part| match part {
                    ToolResultContent::Text { text } => text.len(),
                })
                .sum(),
            ContentBlock::Image { .. } => 0,
        })
        .sum::<usize>();
    let tool_bytes = request
        .tools
        .iter()
        .map(|tool| tool.name.len() + tool.description.len() + tool.input_schema.to_string().len())
        .sum::<usize>();
    u64::try_from(message_bytes.saturating_add(tool_bytes).div_ceil(4)).unwrap_or(u64::MAX)
}
