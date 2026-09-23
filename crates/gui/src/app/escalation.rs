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
        let id = ThreadId::new(format!("escalation-{new_run_id}"));
        let created = !self.sidebar.threads.iter().any(|thread| thread.id == id);
        if created {
            let mut child = ThreadRecord::new(
                id.clone(),
                parent.project_id,
                format!("Orchestrator · {}", parent.title),
            );
            child.parent_thread_id = Some(parent.id.clone());
            child.escalation_source_run_id = Some(source_run_id.into());
            self.sidebar.threads.push(child);
        }
        let thread_id = id.to_string();
        let adopted = !self.transcripts.is_thread_root(new_run_id);
        let has_output = self.transcripts.run(new_run_id).is_some_and(|run| {
            run.entries()
                .iter()
                .any(|entry| !matches!(entry, TranscriptEntry::UserMessage { .. }))
        });
        self.transcripts.adopt_thread_root(&thread_id, new_run_id);
        let bound = self.bind_thread_run(&thread_id, new_run_id);
        if adopted {
            // Do not insert a notice between chunks of an already-started stream.
            if !has_output {
                self.transcripts.push_to_thread(
                    &thread_id,
                    TranscriptEntry::Notice {
                        text: format!(
                            "{} からの escalation: {}\n{}",
                            parent.title, summary.escalation_reason, summary.original_request
                        ),
                    },
                );
            }
        }
        created || bound
    }

    pub(super) fn prepare_escalation_thread(&mut self, source_run_id: &str, run_id: &str) {
        // Live handoff follows the source window's ownership. History replay
        // projects conversations without acquiring any execution authority.
        if let (Some(host), Some(parent), Some(child)) = (
            &self.ownership,
            self.thread_for_run(source_run_id),
            self.thread_for_run(run_id),
        ) && !self.readonly_threads.contains(&parent)
            && host.owned_permit(&parent).is_ok()
            && let Err(error) =
                crate::runtime_sink::finish_chat_start(host, &child, host.start(&child))
        {
            self.transcripts.push_to_thread(
                &child,
                TranscriptEntry::Notice {
                    text: format!("write mode を取得できません: {error}"),
                },
            );
        }
        let id = PanelId::new(format!("agent-{run_id}"));
        if let Some(path) = self.dock.find_tab(&id) {
            self.dock.remove_tab(path);
            self.panels.remove(&id);
            self.equalize_subagent_panes();
        }
    }
}
