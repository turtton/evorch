use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};

use super::{TranscriptEntry, TranscriptModel};

impl TranscriptModel {
    pub(crate) fn apply_thread(&mut self, event: &Event, child_terminal: bool) {
        if child_terminal {
            self.finish_thinking(event);
            if let Some(entry) = self.terminal_notice(event) {
                self.push(entry);
            }
        } else {
            self.apply(event);
        }
    }

    pub(super) fn terminal_notice(&self, event: &Event) -> Option<TranscriptEntry> {
        let (run_id, state) = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Done,
                ..
            }) => (run_id, "completed"),
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Error,
                ..
            }) => (run_id, "failed"),
            _ => return None,
        };
        let name = self
            .agent_names
            .get(run_id)
            .map_or("unknown", String::as_str);
        Some(TranscriptEntry::Notice {
            text: format!("subagent {name} ({run_id}) {state}"),
        })
    }
}
