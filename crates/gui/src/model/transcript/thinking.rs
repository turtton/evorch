use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};

use super::TranscriptModel;

impl TranscriptModel {
    pub fn visible_entry_id(&self, index: usize) -> usize {
        self.first_entry_id + self.view_start.min(self.entries.len()) + index
    }

    pub fn thinking_is_streaming(&self, entry_id: usize) -> bool {
        self.thinking.values().any(|id| *id == entry_id)
    }

    pub(crate) fn finish_history(&mut self) {
        self.thinking.clear();
    }

    pub(super) fn finish_thinking(&mut self, event: &Event) {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Done | AgentRunPhase::Error,
            ..
        }) = &event.kind
        {
            self.thinking.remove(&Some(run_id.clone()));
        }
    }
}
