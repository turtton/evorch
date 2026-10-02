use workspace_ui::{ThreadError, ThreadId, ThreadRecord};

use super::{ConversationFocus, WorkbenchError, WorkbenchState};
use crate::model::tasks::AgentRunSource;
use crate::panes::sidebar::threads::{family_has_running_runs, thread_family};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn toggle_archive(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        let thread = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .ok_or(ThreadError::UnknownThread)?;
        // A family is archived as a unit. Ignore stale child actions as well as
        // clicks on a pinned root, even when they arrive outside the sidebar UI.
        if thread.parent_thread_id.is_some() || (!thread.archived && thread.pinned) {
            return Ok(());
        }
        let archived = !thread.archived;
        let family = thread_family(&self.sidebar.threads, &thread_id);
        // Match the pinned/child no-op contract for stale UI or direct actions.
        // Check the whole family before mutating it: an idle root must not hide
        // a running descendant. Restoring a family is always safe.
        if archived && family_has_running_runs(&self.sidebar.threads, &family, &self.phases) {
            return Ok(());
        }
        for thread in &mut self.sidebar.threads {
            if family.contains(&thread.id) {
                thread.archived = archived;
            }
        }
        if archived
            && self
                .sidebar
                .active_thread
                .as_ref()
                .is_some_and(|active| family.contains(active))
        {
            self.sidebar.active_thread =
                self.sidebar.selected_project.as_ref().and_then(|project| {
                    ThreadRecord::partition_for_project(&self.sidebar.threads, project)
                        .0
                        .first()
                        .map(|thread| thread.id.clone())
                });
            self.transcripts
                .select_thread(self.sidebar.active_thread.as_ref().map(ToString::to_string));
            self.focus = ConversationFocus::Thread;
        }
        self.save_sidebar();
        Ok(())
    }
}
