//! Shared conversation projection for live delivery and persisted event replay.
//!
//! Run ownership and root changes must be applied before transcript routing.
//! This module projects sidebar conversations and transcripts: callers
//! decide whether to persist or perform live UI effects.

use event_bus::{Event, EventKind, LifecycleEvent, OrchestratorEvent};

use workspace_ui::ThreadChatRole;

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Returns whether sidebar conversation identity or its run index changed. Never performs I/O.
    pub(super) fn apply_conversation_event(&mut self, event: &Event) -> bool {
        if let EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) =
            &event.kind
        {
            self.thread_goals
                .insert(snapshot.thread_id.clone(), snapshot.clone());
        }
        let role_changed = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                agent_name,
                parent_run_id: None,
                ..
            }) => match (chat_thread(agent_name), chat_role(agent_name)) {
                (Some(thread), Some(role)) => self.bind_thread_role(thread, role),
                _ => false,
            },
            EventKind::Orchestrator(OrchestratorEvent::GoalCreated { thread_id, .. }) => {
                self.bind_thread_role(thread_id, ThreadChatRole::Orchestrator)
            }
            _ => false,
        };
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
            EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) => {
                self.bind_conversation_run(&snapshot.thread_id, &snapshot.root_run_id, true)
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
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. }) => {
                self.idle_turns.insert(run_id.clone());
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, .. }) => {
                self.idle_turns.remove(run_id);
            }
            _ => {}
        }
        self.transcripts.apply(event);
        changed || role_changed
    }

    fn bind_thread_role(&mut self, thread_id: &str, role: ThreadChatRole) -> bool {
        let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id.to_string() == thread_id)
        else {
            return false;
        };
        let changed = thread.chat_role.is_none();
        thread.chat_role.get_or_insert(role);
        if Some(&thread.id) == self.sidebar.active_thread.as_ref() {
            self.composer.restore_thread_role(thread);
        }
        changed
    }

    pub(super) fn bind_thread_run(&mut self, thread_id: &str, run_id: &str) -> bool {
        self.bind_conversation_run(thread_id, run_id, false)
    }

    pub(super) fn bind_conversation_run(
        &mut self,
        thread_id: &str,
        run_id: &str,
        root: bool,
    ) -> bool {
        let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id.to_string() == thread_id)
        else {
            return false;
        };
        let mut changed = !thread.run_ids.iter().any(|run| run == run_id);
        if changed {
            thread.run_ids.push(run_id.into());
        }
        if root {
            if thread.root_run_id.as_deref() != Some(run_id) {
                thread.root_run_id = Some(run_id.into());
                changed = true;
            }
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

fn chat_role(agent_name: &str) -> Option<ThreadChatRole> {
    let (role, _) = agent_name.strip_prefix("chat:")?.split_once(':')?;
    match role {
        "Worker" => Some(ThreadChatRole::Worker),
        "Orchestrator" => Some(ThreadChatRole::Orchestrator),
        _ => None,
    }
}

fn chat_thread(agent_name: &str) -> Option<&str> {
    let chat = agent_name.strip_prefix("chat:")?;
    Some(chat.split_once(':').map_or(chat, |(_, thread)| thread))
}
