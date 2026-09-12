use crate::event::{DiagnosticEvent, DiagnosticSeverity, Event, EventKind};

use super::state::{EndSpec, StartSpec};
use super::{SpanAction, SpanAttribute, SpanKey, SpanKind, SpanMapper, SpanStatus};

impl SpanMapper {
    pub(super) fn map_event(&mut self, event: &Event) -> Vec<SpanAction> {
        let at = event.meta.wall_clock;
        match &event.kind {
            EventKind::Lifecycle(lifecycle) => self.map_lifecycle(lifecycle, at),
            EventKind::Provider(provider) => self.map_provider(provider, at),
            EventKind::Tool(tool) => self.map_tool(tool, at),
            EventKind::Diagnostic(diagnostic) => self.map_diagnostic(diagnostic, at),
            EventKind::Message(_)
            | EventKind::Ledger(_)
            | EventKind::Usage(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_)
            | EventKind::Ownership(_)
            | EventKind::Snapshot(_) => Vec::new(),
        }
    }

    fn map_diagnostic(
        &mut self,
        event: &DiagnosticEvent,
        at: std::time::SystemTime,
    ) -> Vec<SpanAction> {
        let key = SpanKey::Diagnostic {
            sequence: self.span_sequence.wrapping_add(1),
        };
        let parent = event
            .run_id
            .as_ref()
            .map(|run_id| SpanKey::Agent {
                run_id: run_id.clone(),
            })
            .filter(|key| self.open.contains_key(key));
        let mut attributes = vec![
            SpanAttribute::new("evorch.diagnostic.source", event.source.as_str()),
            SpanAttribute::new("evorch.diagnostic.severity", event.severity.as_str()),
            SpanAttribute::new("evorch.diagnostic.code", event.code.as_str()),
        ];
        if let Some(run_id) = &event.run_id {
            attributes.push(SpanAttribute::new("evorch.agent_run.id", run_id.as_str()));
        }
        if let Some(thread_id) = &event.thread_id {
            attributes.push(SpanAttribute::new("evorch.thread.id", thread_id.as_str()));
        }
        let mut actions = self.start_span(StartSpec {
            key: key.clone(),
            parent,
            name: "evorch.diagnostic".into(),
            kind: SpanKind::Internal,
            at,
            attributes,
        });
        if !actions.is_empty() {
            actions.extend(self.end_span(EndSpec {
                key,
                at,
                status: match event.severity {
                    DiagnosticSeverity::Error => SpanStatus::Error,
                    DiagnosticSeverity::Info | DiagnosticSeverity::Warning => SpanStatus::Unset,
                },
                terminal: Vec::new(),
            }));
        }
        actions
    }
}
