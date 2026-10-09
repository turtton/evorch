use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};

use super::{TranscriptEntry, TranscriptModel};

impl TranscriptModel {
    pub(crate) fn apply_thread(&mut self, event: &Event, child_terminal: bool) {
        if let EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question }) = &event.kind
            && question.run_id != question.root_run_id
        {
            let text = match &question.answer {
                Some(answer) => format!(
                    "Answer sent to subagent {} [{}]: {}",
                    question.run_id, question.id, answer
                ),
                None => format!(
                    "Subagent question from {} reached orchestrator [{}]: {}",
                    question.run_id, question.id, question.title
                ),
            };
            self.push_notice(text);
        } else if child_terminal {
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
                to: to @ AgentRunPhase::Error,
                reason,
                ..
            }) if to.is_cancellation(reason.as_deref()) => (run_id, "cancelled"),
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Error,
                ..
            }) => (run_id, "failed"),
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Stopped,
                ..
            }) => (run_id, "stopped (resumable)"),
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
