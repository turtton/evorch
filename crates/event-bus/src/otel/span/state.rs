use std::time::SystemTime;

use super::{
    OpenSpan, SpanAction, SpanAttribute, SpanDropKind, SpanKey, SpanKind, SpanMapper, SpanStatus,
};

pub(super) struct StartSpec {
    pub(super) key: SpanKey,
    pub(super) parent: Option<SpanKey>,
    pub(super) name: String,
    pub(super) kind: SpanKind,
    pub(super) at: SystemTime,
    pub(super) attributes: Vec<SpanAttribute>,
}

pub(super) struct EndSpec {
    pub(super) key: SpanKey,
    pub(super) at: SystemTime,
    pub(super) status: SpanStatus,
    pub(super) terminal: Vec<SpanAttribute>,
}

impl SpanMapper {
    pub(super) fn start_span(&mut self, spec: StartSpec) -> Vec<SpanAction> {
        if self.open.contains_key(&spec.key) {
            self.record_drop(SpanDropKind::DuplicateSpan, spec.key, spec.at);
            return Vec::new();
        }
        let run_id = match &spec.key {
            SpanKey::Run { run_id } | SpanKey::Agent { run_id } => Some(run_id.clone()),
            SpanKey::Request { .. } | SpanKey::Tool { .. } => {
                spec.parent.as_ref().and_then(|parent| match parent {
                    SpanKey::Agent { run_id } => Some(run_id.clone()),
                    SpanKey::Run { .. }
                    | SpanKey::Request { .. }
                    | SpanKey::Tool { .. }
                    | SpanKey::Session { .. } => None,
                })
            }
            SpanKey::Session { .. } => None,
        };
        if let Err(kind) = self.admit_start(&spec.key, run_id.as_deref(), spec.at) {
            self.add_tombstone(spec.key.clone(), kind);
            self.record_drop(kind, spec.key, spec.at);
            return Vec::new();
        }
        let attributes = self.filter_attributes(&spec.key, spec.attributes, spec.at);
        self.span_sequence = self.span_sequence.wrapping_add(1);
        self.open.insert(
            spec.key.clone(),
            OpenSpan {
                attributes: attributes.clone(),
                in_flight: Vec::new(),
                started_at: spec.at,
                sequence: self.span_sequence,
                run_id,
            },
        );
        vec![SpanAction::Start {
            key: spec.key,
            parent: spec.parent,
            name: spec.name,
            kind: spec.kind,
            start_time: spec.at,
            attributes,
        }]
    }

    pub(super) fn end_span(&mut self, spec: EndSpec) -> Vec<SpanAction> {
        if self.is_tombstoned(&spec.key) {
            return Vec::new();
        }
        let Some(mut span) = self.open.remove(&spec.key) else {
            self.record_drop(SpanDropKind::UnknownSpanEnd, spec.key, spec.at);
            return Vec::new();
        };
        span.attributes.append(&mut span.in_flight);
        span.attributes.extend(spec.terminal);
        let attributes = self.filter_attributes(&spec.key, span.attributes, spec.at);
        vec![SpanAction::End {
            key: spec.key,
            end_time: spec.at,
            status: spec.status,
            final_attributes: attributes,
        }]
    }
}

pub(super) fn terminal_error(error_type: Option<&'static str>) -> Vec<SpanAttribute> {
    error_type
        .map(|value| vec![SpanAttribute::new("error.type", value)])
        .unwrap_or_default()
}
