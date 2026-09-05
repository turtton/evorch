use crate::event::{Event, EventKind};

use super::{SpanAction, SpanMapper};

impl SpanMapper {
    pub(super) fn map_event(&mut self, event: &Event) -> Vec<SpanAction> {
        let at = event.meta.wall_clock;
        match &event.kind {
            EventKind::Lifecycle(lifecycle) => self.map_lifecycle(lifecycle, at),
            EventKind::Provider(provider) => self.map_provider(provider, at),
            EventKind::Tool(tool) => self.map_tool(tool, at),
            EventKind::Message(_)
            | EventKind::Usage(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_) => Vec::new(),
        }
    }
}
