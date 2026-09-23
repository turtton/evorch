//! Shared conversation projection for live delivery and persisted event replay.
//!
//! Run ownership and root changes must be applied before transcript routing.
//! This module projects sidebar conversations and transcripts: callers
//! decide whether to persist or perform live UI effects.

use event_bus::{Event, EventKind, LifecycleEvent, OrchestratorEvent};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Returns whether the sidebar's run index changed. Never performs I/O.
    pub(super) fn apply_conversation_event(&mut self, event: &Event) -> bool {
        let changed = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                source_run_id,
                new_run_id,
                summary,
            }) => self.bind_escalation_thread(source_run_id, new_run_id, summary),
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                agent_name,
                ..
            }) => {
                if let Some(thread) = chat_thread(agent_name) {
                    self.bind_conversation_run(thread, run_id, true)
                } else {
                    let owner = self.thread_for_run(run_id).or_else(|| {
                        parent_run_id
                            .as_deref()
                            .and_then(|parent| self.thread_for_run(parent))
                    });
                    owner.is_some_and(|thread| self.bind_thread_run(&thread, run_id))
                }
            }
            EventKind::Orchestrator(OrchestratorEvent::GoalCreated {
                thread_id,
                root_run_id,
                ..
            }) => self.bind_conversation_run(thread_id, root_run_id, true),
            EventKind::Orchestrator(OrchestratorEvent::ContinuationDispatched {
                trigger_run_id,
                new_run_id,
                ..
            }) => self
                .thread_for_run(trigger_run_id)
                .is_some_and(|thread| self.bind_conversation_run(&thread, new_run_id, true)),
            _ => false,
        };
        self.transcripts.apply(event);
        self.project_escalation_result(event);
        changed
    }

    pub(super) fn bind_thread_run(&mut self, thread_id: &str, run_id: &str) -> bool {
        self.bind_conversation_run(thread_id, run_id, false)
    }

    fn bind_conversation_run(&mut self, thread_id: &str, run_id: &str, root: bool) -> bool {
        let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id.to_string() == thread_id)
        else {
            return false;
        };
        let changed = !thread.run_ids.iter().any(|run| run == run_id);
        if changed {
            thread.run_ids.push(run_id.into());
        }
        if root {
            self.transcripts.bind_thread_root(thread_id, run_id);
        } else {
            self.transcripts.bind_run(run_id, thread_id);
        }
        changed
    }

    pub(super) fn thread_for_run(&self, run_id: &str) -> Option<String> {
        self.sidebar
            .threads
            .iter()
            .find(|thread| thread.run_ids.iter().any(|run| run == run_id))
            .map(|thread| thread.id.to_string())
    }
}

fn chat_thread(agent_name: &str) -> Option<&str> {
    let chat = agent_name.strip_prefix("chat:")?;
    Some(chat.split_once(':').map_or(chat, |(_, thread)| thread))
}
