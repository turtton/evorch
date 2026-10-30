use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};

use super::TranscriptModel;

impl TranscriptModel {
    pub fn visible_entry_id(&self, index: usize) -> usize {
        self.entry_meta
            .get(self.view_start.min(self.entries.len()) + index)
            .map_or(self.next_entry_id, |meta| meta.id)
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
            to: AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error,
            ..
        }) = &event.kind
        {
            self.thinking.remove(&Some(run_id.clone()));
            // A resumed response must not replace partial output from before stop.
            self.streaming_messages.remove(&Some(run_id.clone()));
            self.interrupt_running_tools(run_id);
        }
    }

    /// Marks `run_id`'s calls that are still running as interrupted. The run
    /// has ended, so no result will arrive and a spinner would never stop.
    pub(crate) fn interrupt_running_tools(&mut self, run_id: &str) {
        for (entry, meta) in self.entries.iter_mut().zip(&mut self.entry_meta) {
            if let super::TranscriptEntry::Tool {
                run_id: Some(run),
                status: status @ super::ToolStatus::Running,
                ..
            } = entry
                && run == run_id
            {
                *status = super::ToolStatus::Interrupted;
                meta.revision += 1;
            }
        }
    }

    pub(crate) fn running_tool_runs(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter_map(|entry| match entry {
            super::TranscriptEntry::Tool {
                run_id: Some(run),
                status: super::ToolStatus::Running,
                ..
            } => Some(run.as_str()),
            _ => None,
        })
    }
}
