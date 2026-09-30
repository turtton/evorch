use std::collections::BTreeSet;

use workspace_ui::{ThreadError, ThreadId, ThreadRecord};

use super::{ConversationFocus, WorkbenchError, WorkbenchState};
use crate::model::tasks::AgentRunSource;

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
        let mut family = BTreeSet::from([thread_id]);
        let mut pending = family.iter().cloned().collect::<Vec<_>>();
        while let Some(parent) = pending.pop() {
            for child in self
                .sidebar
                .threads
                .iter()
                .filter(|thread| thread.parent_thread_id.as_ref() == Some(&parent))
            {
                if family.insert(child.id.clone()) {
                    pending.push(child.id.clone());
                }
            }
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
