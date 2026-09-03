use std::time::{Duration, SystemTime};

use super::{
    SpanAction, SpanAttribute, SpanAttributeValue, SpanDropKind, SpanKey, SpanMapper, SpanStatus,
};

const ADMISSION_WINDOW: Duration = Duration::from_secs(60);
const TOMBSTONE_LIMIT: usize = 4096;
const SAMPLE_BUCKETS: u32 = 1_000_000;

/// ADR 0012 span mapper hard limits。
///
/// 公開 API の恒久契約ではなく、回帰テストで固定した運用既定値である。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanBudget {
    pub max_in_flight_spans_per_run: usize,
    pub max_in_flight_spans_global: usize,
    pub max_admitted_spans_per_window: usize,
    pub max_span_lifetime: Duration,
    pub max_attributes_per_span: usize,
    pub max_attribute_bytes_per_span: usize,
    pub max_attribute_value_bytes: usize,
}

impl Default for SpanBudget {
    fn default() -> Self {
        Self {
            max_in_flight_spans_per_run: 128,
            max_in_flight_spans_global: 4096,
            max_admitted_spans_per_window: 10_000,
            max_span_lifetime: Duration::from_secs(30 * 60),
            max_attributes_per_span: 32,
            max_attribute_bytes_per_span: 16 * 1024,
            max_attribute_value_bytes: 1024,
        }
    }
}

impl SpanMapper {
    pub(super) fn audit_lifetimes(&mut self, at: SystemTime) -> Vec<SpanAction> {
        let mut expired: Vec<_> = self
            .open
            .iter()
            .filter(|(_, span)| {
                at.duration_since(span.started_at)
                    .is_ok_and(|age| age > self.budget.max_span_lifetime)
            })
            .map(|(key, span)| (span.sequence, key.clone()))
            .collect();
        expired.sort_unstable_by_key(|(sequence, _)| *sequence);

        let mut actions = Vec::with_capacity(expired.len());
        for (_, key) in expired {
            let Some(mut span) = self.open.remove(&key) else {
                continue;
            };
            span.attributes.append(&mut span.in_flight);
            span.attributes
                .push(SpanAttribute::new("error.type", "span_budget_evicted"));
            let attributes = self.filter_attributes(&key, span.attributes, at);
            self.add_tombstone(key.clone());
            self.record_drop(SpanDropKind::BudgetEvicted, key.clone(), at);
            actions.push(SpanAction::End {
                key,
                end_time: at,
                status: SpanStatus::Error,
                final_attributes: attributes,
            });
        }
        actions
    }

    pub(super) fn sampling_decision(&mut self, run_id: &str, parent_run_id: Option<&str>) -> bool {
        if let Some(decision) = self.sampling_decisions.get(run_id) {
            return *decision;
        }
        let decision = parent_run_id
            .and_then(|parent| self.sampling_decisions.get(parent).copied())
            .unwrap_or_else(|| sampled(run_id, self.sampling_ratio));
        self.sampling_decisions.insert(run_id.to_owned(), decision);
        decision
    }

    pub(super) fn admit_start(
        &mut self,
        key: &SpanKey,
        run_id: Option<&str>,
        at: SystemTime,
    ) -> Result<(), SpanDropKind> {
        if let Some(run_id) = run_id {
            if !self.sampling_decisions.get(run_id).copied().unwrap_or(true) {
                return Err(SpanDropKind::SampledOut);
            }
            if matches!(key, SpanKey::Request { .. } | SpanKey::Tool { .. }) {
                let run_open = self
                    .open
                    .iter()
                    .filter(|(key, span)| {
                        matches!(key, SpanKey::Request { .. } | SpanKey::Tool { .. })
                            && span.run_id.as_deref() == Some(run_id)
                    })
                    .count();
                if run_open >= self.budget.max_in_flight_spans_per_run {
                    return Err(SpanDropKind::BudgetInFlightPerRun);
                }
            }
        }
        if self.open.len() >= self.budget.max_in_flight_spans_global {
            return Err(SpanDropKind::BudgetInFlightGlobal);
        }
        if self.window_is_expired(at) {
            self.window_started_at = Some(at);
            self.admitted_in_window = 0;
        }
        if self.admitted_in_window >= self.budget.max_admitted_spans_per_window {
            return Err(SpanDropKind::BudgetWindow);
        }
        if self.window_started_at.is_none() {
            self.window_started_at = Some(at);
        }
        self.admitted_in_window += 1;
        self.tombstones.remove(key);
        Ok(())
    }

    pub(super) fn filter_attributes(
        &mut self,
        key: &SpanKey,
        attributes: Vec<SpanAttribute>,
        at: SystemTime,
    ) -> Vec<SpanAttribute> {
        let mut kept =
            Vec::with_capacity(attributes.len().min(self.budget.max_attributes_per_span));
        let mut used_bytes = 0_usize;
        for attribute in attributes {
            let value_bytes = attribute_value_bytes(&attribute.value);
            let attribute_bytes = attribute.key.len().saturating_add(value_bytes);
            let admissible = super::span_attrs::validate_attribute(&attribute).is_ok()
                && kept.len() < self.budget.max_attributes_per_span
                && value_bytes <= self.budget.max_attribute_value_bytes
                && used_bytes.saturating_add(attribute_bytes)
                    <= self.budget.max_attribute_bytes_per_span;
            if admissible {
                used_bytes = used_bytes.saturating_add(attribute_bytes);
                kept.push(attribute);
            } else {
                self.record_drop(SpanDropKind::BudgetAttributes, key.clone(), at);
            }
        }
        kept
    }

    pub(super) fn push_in_flight_attribute(
        &mut self,
        key: &SpanKey,
        attribute: SpanAttribute,
        at: SystemTime,
    ) {
        let Some(span) = self.open.get(key) else {
            return;
        };
        let value_bytes = attribute_value_bytes(&attribute.value);
        let used_bytes = span
            .attributes
            .iter()
            .chain(&span.in_flight)
            .map(attribute_bytes)
            .fold(0_usize, usize::saturating_add);
        let count = span.attributes.len().saturating_add(span.in_flight.len());
        let admissible = super::span_attrs::validate_attribute(&attribute).is_ok()
            && count < self.budget.max_attributes_per_span
            && value_bytes <= self.budget.max_attribute_value_bytes
            && used_bytes.saturating_add(attribute_bytes(&attribute))
                <= self.budget.max_attribute_bytes_per_span;
        if admissible {
            if let Some(span) = self.open.get_mut(key) {
                span.in_flight.push(attribute);
            }
        } else {
            self.record_drop(SpanDropKind::BudgetAttributes, key.clone(), at);
        }
    }

    pub(super) fn is_tombstoned(&self, key: &SpanKey) -> bool {
        self.tombstones.contains_key(key)
    }

    pub(super) fn add_tombstone(&mut self, key: SpanKey) {
        self.tombstone_sequence = self.tombstone_sequence.wrapping_add(1);
        self.tombstones.insert(key, self.tombstone_sequence);
        if self.tombstones.len() > TOMBSTONE_LIMIT
            && let Some(oldest) = self
                .tombstones
                .iter()
                .min_by_key(|(_, sequence)| **sequence)
                .map(|(key, _)| key.clone())
        {
            self.tombstones.remove(&oldest);
        }
    }

    fn window_is_expired(&self, at: SystemTime) -> bool {
        self.window_started_at.is_some_and(|started| {
            at.duration_since(started)
                .is_ok_and(|elapsed| elapsed >= ADMISSION_WINDOW)
        })
    }
}

fn sampled(run_id: &str, ratio: f64) -> bool {
    let bucket_u64 = fnv1a(run_id.as_bytes()) % u64::from(SAMPLE_BUCKETS);
    let Ok(bucket) = u32::try_from(bucket_u64) else {
        return false;
    };
    f64::from(bucket) < ratio * f64::from(SAMPLE_BUCKETS)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn attribute_value_bytes(value: &SpanAttributeValue) -> usize {
    match value {
        SpanAttributeValue::Str(value) => value.len(),
        SpanAttributeValue::Strings(values) => values.iter().map(String::len).sum(),
        SpanAttributeValue::I64(_) | SpanAttributeValue::F64(_) => 8,
        SpanAttributeValue::Bool(_) => 1,
    }
}

fn attribute_bytes(attribute: &SpanAttribute) -> usize {
    attribute
        .key
        .len()
        .saturating_add(attribute_value_bytes(&attribute.value))
}
