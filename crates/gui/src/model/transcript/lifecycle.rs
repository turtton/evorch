use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};

use super::{TranscriptEntry, TranscriptModel};

impl TranscriptModel {
    pub(crate) fn apply_thread(&mut self, event: &Event, child_terminal: bool) {
        if let EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question }) = &event.kind
            && question.run_id != question.root_run_id
        {
            let name = self.agent_names.get(&question.run_id);
            // Who answered is shown; an unknown origin keeps the plain wording.
            let by = match &question.answer_origin {
                Some(event_bus::AnswerOrigin::Host) => " from you",
                Some(event_bus::AnswerOrigin::ParentAgent { .. }) => " from parent agent",
                None => "",
            };
            let text = match (&question.answer, name) {
                (Some(answer), Some(name)) => {
                    format!("Answer{by} sent to subagent {name}: {answer}")
                }
                (Some(answer), None) => format!("Answer{by} sent to a subagent: {answer}"),
                (None, Some(name)) => format!(
                    "Subagent question from {name} reached orchestrator: {}",
                    question.title
                ),
                (None, None) => {
                    format!("Subagent question reached orchestrator: {}", question.title)
                }
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

    /// Whether a `delegate` call is still spawning, so its card covers the start.
    pub(super) fn delegate_in_flight(&self) -> bool {
        self.entries.iter().rev().take(32).any(|entry| {
            matches!(entry, TranscriptEntry::Tool { tool_name, status: super::ToolStatus::Running, .. }
                if tool_name == "delegate")
        })
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
            }) => (run_id, "stopped"),
            _ => return None,
        };
        let text = match self.agent_names.get(run_id) {
            Some(name) => format!("subagent {name} {state}"),
            None => format!("a subagent {state}"),
        };
        Some(TranscriptEntry::Notice { text })
    }
}
