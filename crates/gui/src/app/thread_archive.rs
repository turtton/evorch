use workspace_ui::{ThreadError, ThreadId, ThreadRecord};

use super::{ConversationFocus, WorkbenchError, WorkbenchState};
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn toggle_archive(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        let thread = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            .ok_or(ThreadError::UnknownThread)?;
        thread.archived = !thread.archived;
        if thread.archived && self.sidebar.active_thread.as_ref() == Some(&thread_id) {
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
