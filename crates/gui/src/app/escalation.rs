use event_bus::EscalationMemoSummary;
use workspace_ui::{PanelId, ThreadId, ThreadRecord};

use super::WorkbenchState;
use crate::model::{tasks::AgentRunSource, transcript::TranscriptEntry};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn bind_escalation_thread(
        &mut self,
        source_run_id: &str,
        new_run_id: &str,
        summary: &EscalationMemoSummary,
    ) -> bool {
        let Some(parent) = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.run_ids.iter().any(|run| run == source_run_id))
            .cloned()
        else {
            return false;
        };
        let id = ThreadId::new(event_bus::escalation_thread_id(new_run_id));
        let created = !self.sidebar.threads.iter().any(|thread| thread.id == id);
        if created {
            self.create_escalation_thread(&id, &parent, source_run_id);
        }
        let thread_id = id.to_string();
        let adopted = !self.transcripts.is_thread_root(new_run_id);
        let has_output = self.transcripts.run(new_run_id).is_some_and(|run| {
            run.entries()
                .iter()
                .any(|entry| !matches!(entry, TranscriptEntry::UserMessage { .. }))
        });
        self.transcripts.adopt_thread_root(&thread_id, new_run_id);
        let bound = self.bind_conversation_run(&thread_id, new_run_id, true);
        if adopted {
            let inherited = self
                .user_questions
                .values()
                .filter(|question| {
                    question.answer.is_none()
                        && question.is_user_visible()
                        && question
                            .recipient_run_ids
                            .iter()
                            .any(|run| run == new_run_id)
                })
                .map(|question| question.id.as_str())
                .collect::<Vec<_>>();
            if !inherited.is_empty() {
                self.transcripts.push_to_thread(
                    &parent.id.to_string(),
                    TranscriptEntry::Notice {
                        text: format!(
                            "Unanswered questions ({}) moved to Orchestrator · {} ({thread_id}, {new_run_id}). Answer them here or there under the same IDs.",
                            inherited.join(", "), parent.title
                        ),
                    },
                );
            }
            // Do not insert a notice between chunks of an already-started stream.
            if !has_output {
                self.transcripts.push_to_thread(
                    &thread_id,
                    TranscriptEntry::Notice {
                        text: format!(
                            "Escalated from {}: {}\n{}",
                            parent.title, summary.escalation_reason, summary.original_request
                        ),
                    },
                );
            }
        }
        created || bound
    }

    /// Creates the escalated conversation as a child of `parent` and settles its
    /// archive and pin state against the newest sidebar under the save lock. A
    /// history event cannot fail, so when the sidebar cannot be locked or read the
    /// child is still created, in the neutral state (not archived, not pinned,
    /// revision 0) and flagged `state_pending`. The flag is saved with the record: any
    /// window that can lock the file settles it later ([`Self::retry_pending_family_state`]),
    /// also after a restart, and until then the record is not told to the database.
    fn create_escalation_thread(
        &mut self,
        id: &ThreadId,
        parent: &ThreadRecord,
        source_run_id: &str,
    ) {
        let mut child = ThreadRecord::new(
            id.clone(),
            parent.project_id.clone(),
            format!("Orchestrator · {}", parent.title),
        );
        child.chat_role = Some(workspace_ui::ThreadChatRole::Orchestrator);
        // The runtime copies the declaration at handoff; the record keeps it
        // across restarts (ADR 0037).
        child.extra_read_dirs.clone_from(&parent.extra_read_dirs);
        child.parent_thread_id = Some(parent.id.clone());
        child.escalation_source_run_id = Some(source_run_id.into());
        child.state_pending = true;
        self.sidebar.threads.push(child.clone());
        let settled = self.shared_sidebar_transact_detached(|latest| {
            let threads = &mut latest.sidebar.threads;
            // The local child is the record to settle: it joins the newest state.
            if !threads.iter().any(|thread| &thread.id == id) {
                threads.push(child.clone());
            }
            super::thread_archive::settle_pending_threads(threads);
            threads
                .iter()
                .find(|thread| &thread.id == id)
                .cloned()
                .ok_or(workspace_ui::ThreadError::UnknownThread)
        });
        match settled {
            Ok(settled) => {
                if let Some(local) = self.sidebar.threads.iter_mut().find(|t| &t.id == id) {
                    local.inherit_series_of(&settled);
                }
            }
            Err(error) => {
                tracing::warn!(%error, thread = %id, "escalated thread created before its archive state could be settled; it is saved as pending");
            }
        }
    }

    pub(super) fn prepare_escalation_thread(&mut self, _source_run_id: &str, run_id: &str) {
        // Runtime acquires the child permit before starting the orchestrator.
        // Conversation projection, including replay, grants no execution authority.
        let id = PanelId::new(format!("agent-{run_id}"));
        if let Some(path) = self.dock.find_tab(&id) {
            self.dock.remove_tab(path);
            self.panels.remove(&id);
            self.equalize_subagent_panes();
        }
    }
}
