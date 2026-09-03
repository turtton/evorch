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
        self.open.insert(
            spec.key.clone(),
            OpenSpan {
                attributes: spec.attributes.clone(),
                in_flight: Vec::new(),
            },
        );
        vec![SpanAction::Start {
            key: spec.key,
            parent: spec.parent,
            name: spec.name,
            kind: spec.kind,
            start_time: spec.at,
            attributes: spec.attributes,
        }]
    }

    pub(super) fn end_span(&mut self, spec: EndSpec) -> Vec<SpanAction> {
        let Some(mut span) = self.open.remove(&spec.key) else {
            self.record_drop(SpanDropKind::UnknownSpanEnd, spec.key, spec.at);
            return Vec::new();
        };
        span.attributes.append(&mut span.in_flight);
        span.attributes.extend(spec.terminal);
        vec![SpanAction::End {
            key: spec.key,
            end_time: spec.at,
            status: spec.status,
            final_attributes: span.attributes,
        }]
    }
}

pub(super) fn terminal_error(error_type: Option<&'static str>) -> Vec<SpanAttribute> {
    error_type
        .map(|value| vec![SpanAttribute::new("error.type", value)])
        .unwrap_or_default()
}
